use maxminddb::{Mmap, Reader, geoip2};
use std::{
    io,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};
use time::OffsetDateTime;

pub type Place = (Option<String>, Option<String>, Option<String>);

/// GeoIP 查询,库文件可热替换;未配置路径或文件不存在时一律返回空。
pub struct Geo {
    pub path: Option<PathBuf>,
    reader: RwLock<Option<Arc<Reader<Mmap>>>>,
}

impl Geo {
    pub fn new(path: Option<PathBuf>) -> Self {
        let g = Geo { path, reader: RwLock::new(None) };
        g.reload();
        g
    }

    pub fn reload(&self) {
        let Some(p) = &self.path else { return };
        if !p.exists() {
            return;
        }
        // SAFETY: 库文件只会被 download() 以 rename 方式原子替换,不会原地截断。
        match unsafe { Reader::open_mmap(p) } {
            Ok(r) => {
                *self.reader.write().unwrap() = Some(Arc::new(r));
                crate::i18n::log_tr!(info, "GeoIP database loaded: {}", "GeoIP 库已加载: {}", p.display());
            }
            Err(e) => crate::i18n::log_tr!(warn, "cannot open the GeoIP database {}: {e}", "GeoIP 库无法打开 {}: {e}", p.display()),
        }
    }

    pub fn lookup(&self, ip: IpAddr) -> Place {
        let Some(r) = self.reader.read().unwrap().clone() else { return Default::default() };
        let Ok(res) = r.lookup(ip) else { return Default::default() };
        let Ok(Some(c)) = res.decode::<geoip2::City>() else { return Default::default() };
        let s = |v: Option<&str>| v.filter(|v| !v.is_empty()).map(String::from);
        (
            s(c.country.iso_code),
            s(c.subdivisions.first().and_then(|d| d.names.english)),
            s(c.city.names.english),
        )
    }

    fn age(&self) -> Option<Duration> {
        let m = std::fs::metadata(self.path.as_ref()?).ok()?.modified().ok()?;
        SystemTime::now().duration_since(m).ok()
    }
}

/// 启动时库不存在则下载,之后每天检查一次,超过 30 天就更新(DB-IP 每月初发布)。
pub async fn auto_update(geo: Arc<Geo>) {
    loop {
        if geo.age().is_none_or(|a| a > Duration::from_secs(30 * 86400)) {
            let path = geo.path.clone().unwrap();
            match tokio::task::spawn_blocking(move || download(&path)).await {
                Ok(Ok(())) => geo.reload(),
                Ok(Err(e)) => crate::i18n::log_tr!(warn, "GeoIP download failed: {e}", "GeoIP 下载失败: {e}"),
                Err(e) => crate::i18n::log_tr!(warn, "GeoIP download task error: {e}", "GeoIP 下载任务异常: {e}"),
            }
        }
        tokio::time::sleep(Duration::from_secs(86400)).await;
    }
}

fn download(path: &Path) -> io::Result<()> {
    let now = OffsetDateTime::now_utc();
    let (y, m) = (now.year(), now.month() as u8);
    let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
    // 当月文件可能还没发布,回退到上个月
    let mut last_err = io::Error::other("no candidate");
    for (y, m) in [(y, m), (py, pm)] {
        let url = format!("https://download.db-ip.com/free/dbip-city-lite-{y}-{m:02}.mmdb.gz");
        match fetch_gz(&url, path) {
            Ok(()) => {
                crate::i18n::log_tr!(info, "GeoIP database updated: {url}", "GeoIP 库已更新: {url}");
                return Ok(());
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn fetch_gz(url: &str, dest: &Path) -> io::Result<()> {
    let resp = ureq::get(url).call().map_err(io::Error::other)?;
    let mut gz = flate2::read::GzDecoder::new(resp.into_body().into_reader());
    let tmp = dest.with_extension("tmp");
    io::copy(&mut gz, &mut std::fs::File::create(&tmp)?)?;
    // 先确认能打开再替换
    Reader::open_readfile(&tmp).map_err(io::Error::other)?;
    std::fs::rename(&tmp, dest)
}
