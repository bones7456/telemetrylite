# 供分析用的读取 API(`/r/v1`)

[English](READ_API.md) | 简体中文

只读的 JSON API,让 AI agent(Claude Code、Codex 等)和脚本不用打开管理界面就能分析某个 App 的数据。

## 读取 key

- 在管理界面签发:App → 设置 →「读取 API key」。以 `tlr_` 开头,只显示一次。
- 绑定**一个 App**(App 由 key 决定,不需要传 app id),只能调用 `/r/v1/*`。它不能上报事件(写入接口返回 `403`),打不开管理 API,也不能管理 key。
- 用 `Authorization: Bearer tlr_…`(或 `x-api-key`)发送,不要放进 URL。随时可以吊销,立即生效。
- 限制:每个 key 每分钟 120 次请求(`429` + `Retry-After`),同时最多执行 4 个查询,单个查询超过 5 秒会被取消(`504 query_timeout`)。

## 让 agent 来分析

管理界面 → App → 设置 →「让 AI agent 分析数据」:签发读取 key,点「复制提示词」,粘贴给你的 agent,并在末尾写上问题。提示词刻意很短,因为 API 会自己说明用法。

```text
curl -s -H "Authorization: Bearer $KEY" https://telemetry.example.com/r/v1/
```

## 接口

| 接口 | 作用 |
|---|---|
| `GET /r/v1/` | 自描述:App、数据起止时间、平台、事件类型(近 30 天)、自定义参数名、每个接口及其参数、约定。先调用它。 |
| `GET /r/v1/overview` | 一次调用看全貌:用户 / 事件 / 会话及与上一周期的变化、新用户、趋势、热门事件、各维度分布(App 版本、系统版本、机型、语言、国家、城市,含用户占比)、错误 Top、按周留存。 |
| `GET /r/v1/events` | 原始事件,新的在前。自定义参数在 `params` 下。用 `cursor` 翻页。 |
| `GET /r/v1/query` | 聚合:`metric`(`events`、`users`、`sessions`,可多个)按最多 3 个字段分组,可选按 `interval`(`hour`、`day`、`week`、`month`)分桶。 |

通用参数:`days`(默认 7)、`since` / `until`(RFC 3339、`YYYY-MM-DD` 或毫秒时间戳)、`test`(`0` 正式数据,默认;`1` 测试模式;`all`)、`platform`(如 `iOS`,不区分大小写)、`tz`(分桶用的相对 UTC 东移分钟数,默认 0)、`format`(`json`;`events` 和 `query` 另支持 `csv` / `md`)。

`events` 和 `query` 还支持 `type`(精确事件名)和可重复的 `where=<字段>:<值>`(取交集)。`events`:`limit`(最大 500)、`cursor`。`query`:`metric`、`group_by`、`interval`、`order`(`value` 或 `key`)、`limit`(最大 1000)。

`group_by` / `where` 可用字段:`type`、`platform`、`app_version`、`os_version`、`device_model`、`locale`、`country`、`city`、`build`、`sdk`,以及任意自定义参数 `payload.<key>`(参数名见 `/r/v1/` 的 `custom_parameters`)。

```text
# 近 90 天,按套餐和国家统计每周的购买
/r/v1/query?type=Purchase.completed&metric=events,users&group_by=payload.plan,country&interval=week&days=90
# 某个套餐的原始事件,紧凑表格
/r/v1/events?type=Purchase.completed&where=payload.plan:yearly&limit=20&format=md
```

## 为 agent 设计

- 自描述(`/r/v1/`),提示词不会过期。
- 每个响应都带 `meta`:实际生效的时间窗口、数据类型(正式 / 测试 / 全部)、平台筛选、时区和 `truncated`。不会静默截断。
- 未知参数会被拒绝而不是忽略,写错参数名不会让 agent 分析错东西。
- 错误格式为 `{"error": "<code>", "message": "...", "hint": "..."}`,`hint` 说明如何修正请求。
- 时间戳为 ISO 8601 UTC。大表格用 `csv` / `md` 比 JSON 省很多 token。
- 没有原始 SQL:字段走白名单,自定义参数名作为绑定参数传入。

## 隐私

用户是加盐哈希,不存 IP。读取 key 能看到 `payload` 里的全部内容,即你的 App 发送的任何自定义参数,所以不要把个人信息放进去。
