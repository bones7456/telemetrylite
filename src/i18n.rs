//! Log/message language: English by default, Chinese when `TL_LANG` starts with "zh".

use std::sync::OnceLock;

pub fn zh() -> bool {
    static ZH: OnceLock<bool> = OnceLock::new();
    *ZH.get_or_init(|| std::env::var("TL_LANG").is_ok_and(|v| v.starts_with("zh")))
}

/// Picks a static message by language.
pub fn tr(en: &'static str, zh_: &'static str) -> &'static str {
    if zh() { zh_ } else { en }
}

/// `log_tr!(warn, "english {x}", "中文 {x}", extra_args...)`
macro_rules! log_tr {
    ($lvl:ident, $en:literal, $zh:literal $(, $arg:expr)*) => {
        if $crate::i18n::zh() { tracing::$lvl!($zh $(, $arg)*) } else { tracing::$lvl!($en $(, $arg)*) }
    };
}
pub(crate) use log_tr;
