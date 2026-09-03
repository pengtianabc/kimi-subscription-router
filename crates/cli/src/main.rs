//! kimi-switch（Kimi Code 裁剪版）CLI 入口。
//!
//! 命令面（刻意保持最小）：
//! - `kimi-switch`              — 默认入口：同步本地激活账号，列出 Kimi 账号 + 5h/7d 额度。
//! - `kimi-switch login kimi`   — 导入当前本机 Kimi Code 已登录账号
//!   （`~/.kimi-code/credentials/kimi-code.json`）。
//! - `kimi-switch swap <id|N>`  — 切换激活账号。原子写 + 快照回滚，不依赖网络/quota。
//!   无参数时只打印编号列表，不做切换。
//! - `kimi-switch rm <id|N>`    — 删除账号（registry + 凭证仓库 + 墓碑）。
//! - `kimi-switch auto`         — 切到 5h 额度剩余最多的账号（dry-run 只打印）。
//! - `kimi-switch watch 'bash run.sh'` — 循环监控额度：当前账号 5h 用光后，等有额度的
//!   账号就自动切换并重新执行命令（`--cnt` 限制生效次数，屏幕每次轮询清屏）。
//!
//! `<id>` 是账号 id（Kimi user_id）、label 或 `kimi/<id>`；`<N>` 是默认入口
//! 显示的编号（1 起）。

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, Utc};
use unicode_width::UnicodeWidthChar;
use clap::{Parser, Subcommand};
use kimi_switch_core::paths::AppPaths;
use kimi_switch_core::{
    settings, Account, AccountRegistry, AuditEvent, AuditLog, CredentialStore, FileStore,
    KeyringStore, Provider, Quota, QuotaCache, QuotaWindow, RemovedAccounts,
};
use kimi_switch_kimi::KimiProvider;

/// registry `extra` 键：该账号是否参与自动路由（auto 切换候选）。与 GUI 保持一致。
const EXTRA_ROUTING_ENABLED: &str = "routing_enabled";
/// registry `extra` 键：订阅到期日（YYYY-MM-DD）。与 GUI 保持一致。
const EXTRA_SUBSCRIPTION_EXPIRES_ON: &str = "subscription_expires_on";

#[derive(Parser)]
#[command(
    name = "kimi-switch",
    version,
    about = "Manage and swap between multiple Kimi Code accounts.",
    long_about = "Run `kimi-switch` with no arguments to list Kimi accounts and their quota. \
                  Use `login kimi` / `swap` / `rm` for explicit actions."
)]
struct Cli {
    /// Log level (equivalent to RUST_LOG).
    #[arg(long, global = true, default_value = "warn")]
    log: String,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import the currently signed-in Kimi Code account.
    Login {
        /// Provider to log in: only `kimi` is supported.
        provider: String,
    },

    /// Swap to <id|N>. With no argument, prints numbered accounts and exits.
    Swap {
        /// Account index (e.g. `1`), id, label, or `kimi/<id>`.
        id: Option<String>,
    },

    /// Remove <id|N> from the registry and credential store.
    Rm {
        /// Account index (e.g. `1`), id, label, or `kimi/<id>`.
        id: String,
    },

    /// List accounts with quota and current-active marker (also the default action).
    List {
        /// Emit machine-readable JSON instead of the formatted table.
        #[arg(long)]
        json: bool,
    },

    /// Update account metadata for <id|N>.
    Set {
        /// Account index (e.g. `1`), id, label, or `kimi/<id>`.
        id: String,
        /// Friendly alias shown in listings.
        #[arg(long)]
        label: Option<String>,
        /// Manual priority; lower number wins ties in auto-switch.
        #[arg(long)]
        priority: Option<i32>,
        /// Toggle participation in auto-switch / auto-routing.
        #[arg(long = "routing-enabled")]
        routing_enabled: Option<bool>,
        /// Subscription expiry as YYYY-MM-DD (empty string clears it).
        #[arg(long = "subscription-expires-on")]
        subscription_expires_on: Option<String>,
    },

    /// Auto-switch to the account with the most remaining short-window (5h) quota.
    Auto {
        /// Print the chosen account without actually switching.
        #[arg(long)]
        dry_run: bool,
    },

    /// Watch quota and keep running <command> on whichever account has 5h quota.
    ///
    /// The program blocks and polls quota on an interval. When the active account's
    /// 5h quota runs out, it waits until some account has quota available, auto-switches
    /// to it, then re-runs <command>. The screen is cleared between polls instead of
    /// spam-printing. <command> is checked for usability (script exists + shell syntax)
    /// before the first run.
    Watch {
        /// Shell command / script to run when an account has quota.
        /// Example: `kimi-switch watch 'bash run.sh'`.
        command: String,
        /// 生效次数：执行多少次后退出（默认无限）。
        #[arg(short = 'c', long = "cnt")]
        cnt: Option<usize>,
        /// 轮询间隔（秒）。
        #[arg(long, default_value_t = 30)]
        interval: u64,
    },
}

/// 进程级共享上下文：明文文件凭证仓库 + registry + Kimi provider。
struct AppContext {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    kimi: Arc<KimiProvider>,
    audit: AuditLog,
}

impl AppContext {
    fn build() -> Result<Self> {
        let paths = AppPaths::resolve()?;
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::with_legacy_keyring(
            paths.credentials_file(),
            KeyringStore::new(),
        ));
        let registry = Arc::new(AccountRegistry::from_default_paths()?);
        let kimi = Arc::new(kimi_switch_kimi::new(store.clone(), registry.clone()));
        let audit = AuditLog::from_default_paths()?;
        Ok(Self {
            store,
            registry,
            kimi,
            audit,
        })
    }

    /// 账号显示顺序（只有 kimi 一个 provider）。`kimi-switch`、`swap N`、`rm N` 共用同一编号映射。
    fn list_ordered(&self) -> Result<Vec<Account>> {
        Ok(self.registry.list_by_provider("kimi")?)
    }

    fn load_removed() -> Result<RemovedAccounts> {
        Ok(RemovedAccounts::load(&AppPaths::resolve()?.removed_file()))
    }
}

/// 把用户传入的引用解析到具体账号：纯数字 N 取显示顺序第 N 个；
/// 否则按 id / label / `kimi/<id>` 走 `find_unique`。
fn resolve_account(ctx: &AppContext, input: &str) -> Result<Account> {
    let trimmed = input.trim();
    if let Ok(n) = trimmed.parse::<usize>() {
        if n == 0 {
            anyhow::bail!("invalid account index 0; numbering starts at 1");
        }
        let ordered = ctx.list_ordered()?;
        return ordered.into_iter().nth(n - 1).with_context(|| {
            format!("no account at index {n}; run `kimi-switch` to see the list")
        });
    }
    ctx.registry
        .find_unique(trimmed)?
        .filter(|a| a.provider == "kimi")
        .with_context(|| format!("account not found: {trimmed}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(cli.log.clone()));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // 启动时加载 config.toml（缺失 / 解析失败时沿用默认值 + warn）。
    if let Err(e) = settings::reload_from_file() {
        tracing::warn!(err = %e, "load config failed; using built-in defaults");
    }

    let ctx = AppContext::build()?;

    match cli.cmd {
        None => status(&ctx).await,
        Some(Cmd::Login { provider }) => login(&ctx, &provider),
        Some(Cmd::Swap { id }) => swap(&ctx, id.as_deref()).await,
        Some(Cmd::Rm { id }) => rm(&ctx, &id).await,
        Some(Cmd::List { json }) => list(&ctx, json).await,
        Some(Cmd::Set {
            id,
            label,
            priority,
            routing_enabled,
            subscription_expires_on,
        }) => set(&ctx, &id, label, priority, routing_enabled, subscription_expires_on).await,
        Some(Cmd::Auto { dry_run }) => auto(&ctx, dry_run).await,
        Some(Cmd::Watch {
            command,
            cnt,
            interval,
        }) => watch(&ctx, command, cnt, interval).await,
    }
}

// ---------------------------------------------------------------------------
// 默认入口：账号列表 + 额度
// ---------------------------------------------------------------------------

/// 单个账号的额度查询结果。
#[derive(Clone)]
enum QuotaOutcome {
    Ready(Vec<Quota>),
    /// 查询失败但有仍有效的旧缓存。
    Stale(Vec<Quota>),
    Failed(String),
}

/// 账号资料里对用户可读的字段（来自 Provider profile 接口）。
#[derive(Clone)]
struct ProfileView {
    email: Option<String>,
    display_label: Option<String>,
}

/// `gather` 的一行：账号 + 额度 + 资料。
struct Row {
    account: Account,
    quota: Option<QuotaOutcome>,
    profile: Option<ProfileView>,
}

impl Row {
    /// 取当前有效的额度列表（仅 Ready/Stale 有）。
    fn quotas(&self) -> Option<&[Quota]> {
        match &self.quota {
            Some(QuotaOutcome::Ready(q)) | Some(QuotaOutcome::Stale(q)) => Some(q),
            _ => None,
        }
    }

    /// 用于表格展示的用户名：优先 email，其次 display_label，再回落 label。
    fn display_name(&self) -> String {
        self.profile
            .as_ref()
            .and_then(|p| p.email.clone())
            .or_else(|| self.profile.as_ref().and_then(|p| p.display_label.clone()))
            .unwrap_or_else(|| self.account.label.clone())
    }
}

/// 拉齐本地激活账号 + 列出账号 + 并发查额度与资料（带缓存节流）。
/// 返回 `Row` 列表，供默认 `status`、`list` 与 `auto` 共用。
async fn gather(ctx: &AppContext) -> Result<Vec<Row>> {
    // 1. 自动导入/对齐本地激活账号（`rm` 过的账号有墓碑，跳过）。
    sync_local_active(ctx);

    // 2. 列出账号。
    let accounts = ctx.list_ordered()?;
    if accounts.is_empty() {
        return Ok(Vec::new());
    }

    // 3. 并发查额度 + 资料（缓存节流 + 失败退避，避免高频打 usage 端点）。
    let cache_path = AppPaths::resolve()?.quota_cache_file();
    let mut cache = QuotaCache::load(&cache_path);
    let quota_cfg = settings::current().quota.clone();
    let min_refresh = Duration::from_millis(quota_cfg.min_refresh_interval_ms);
    let backoff_cap = Duration::from_millis(quota_cfg.failure_backoff_max_ms);

    let mut outcomes: Vec<Option<QuotaOutcome>> = vec![None; accounts.len()];
    let mut profiles: Vec<Option<ProfileView>> = vec![None; accounts.len()];
    let mut jobs = Vec::new();
    for (idx, account) in accounts.iter().enumerate() {
        if let Some(entry) = cache.fresh("kimi", &account.id.0, min_refresh) {
            outcomes[idx] = Some(QuotaOutcome::Ready(entry.quotas));
            continue;
        }
        if let Some(failure) =
            cache.in_failure_backoff("kimi", &account.id.0, min_refresh, backoff_cap)
        {
            let error = failure.error.clone();
            outcomes[idx] = Some(match cache.get("kimi", &account.id.0) {
                Some(entry) if !kimi_switch_core::is_authentication_failure(&error) => {
                    QuotaOutcome::Stale(entry.quotas)
                }
                _ => QuotaOutcome::Failed(error),
            });
            continue;
        }
        jobs.push((idx, account.id.clone()));
    }

    let mut handles = Vec::new();
    for (idx, id) in jobs {
        let kimi = ctx.kimi.clone();
        handles.push(tokio::spawn(async move {
            // 一次拿配额 + 资料，parked 账号只刷新一次 access token。
            let result = kimi.fetch_quota_and_profile(&id).await.map_err(|e| e.to_string());
            (idx, id, result)
        }));
    }
    for handle in handles {
        let (idx, id, result) = handle.await?;
        match result {
            Ok((quotas, profile)) => {
                cache.set("kimi", &id.0, quotas.clone());
                outcomes[idx] = Some(QuotaOutcome::Ready(quotas));
                profiles[idx] = Some(ProfileView {
                    email: profile.email,
                    display_label: profile.display_label,
                });
            }
            Err(error) => {
                cache.record_failure("kimi", &id.0, &error);
                outcomes[idx] = Some(match cache.get("kimi", &id.0) {
                    Some(entry) if !kimi_switch_core::is_authentication_failure(&error) => {
                        QuotaOutcome::Stale(entry.quotas)
                    }
                    _ => QuotaOutcome::Failed(error),
                });
            }
        }
    }
    cache.save(&cache_path);

    Ok(accounts
        .into_iter()
        .enumerate()
        .map(|(i, a)| Row {
            account: a,
            quota: std::mem::take(&mut outcomes[i]),
            profile: std::mem::take(&mut profiles[i]),
        })
        .collect())
}

/// 字符显示宽度用 `unicode-width` 计算（`UnicodeWidthStr::width` / `UnicodeWidthChar::width`）：
/// CJK / 全角 / emoji 占 2 列，拉丁扩展、Tai Le 等普通 Unicode 字母仍占 1 列，
/// 这样含 emoji 的账号名才能在表格里正确对齐。

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

/// 单字符显示宽度。基础值来自 `unicode-width`（CJK/emoji/全角 = 2，拉丁 = 1）。
///
/// 例外：Tai Le / New Tai Lue / Balinese / Cham / Tai Tham 等文字在 UCD 里标记为
/// EAW=N（→1），但常被当作「颜文字」装饰（如 `ᥬ😳᭄`）使用，多数终端按 2 列渲染
/// （或缺失字形时显示成占 2 格的豆腐块）。这里统一按 2 列计，使账号名列边框与实际
/// 渲染对齐——字符本身原样输出，不改变文案。
fn char_width(c: char) -> usize {
    let base = UnicodeWidthChar::width(c).unwrap_or(1);
    if base == 1 && is_decorative_wide_script(c) {
        2
    } else {
        base
    }
}

/// 这些文字块在终端里普遍被渲染为 2 列，需要补宽以保证表格对齐。
fn is_decorative_wide_script(c: char) -> bool {
    let u = c as u32;
    matches!(
        u,
        0x1950..=0x197F   // Tai Le
        | 0x1980..=0x19DF // New Tai Lue
        | 0x1A20..=0x1AAF // Tai Tham
        | 0x1B00..=0x1B7F // Balinese
        | 0xAA00..=0xAA5F // Cham
    )
}

/// 整串显示宽度：逐字符用 [`char_width`] 求和。
fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 按显示宽度截断（超出加 `…`）并补齐到 `width` 列。`align` 控制左右 / 居中。
fn pad_cell(s: &str, width: usize, align: Align) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    if w < str_width(s) {
        // 被截断，尽量补一个省略号（U+2026 占 1 列）。
        if w < width {
            out.push('…');
            w += 1;
        }
    }
    let pad = width.saturating_sub(w);
    match align {
        Align::Left => format!("{}{}", out, " ".repeat(pad)),
        Align::Center => {
            let left = pad / 2;
            format!("{}{}{}", " ".repeat(left), out, " ".repeat(pad - left))
        }
        Align::Right => format!("{}{}", " ".repeat(pad), out),
    }
}

/// 单元格：在 `width` 列宽内左右各留一个空格，即 ` content `（列宽已含两侧空格）。
fn cell(content: &str, width: usize, align: Align) -> String {
    format!(" {} ", pad_cell(content, width.saturating_sub(2), align))
}

/// 表格分隔行：`+----+----+...+`。
fn sep_line(widths: &[usize]) -> String {
    let mut s = String::new();
    for w in widths {
        s.push('+');
        s.push_str(&"-".repeat(*w));
    }
    s.push('+');
    s
}

/// 把剩余时间渲染为两单位紧凑相对形式：`45m` / `3h15m` / `1d4h` / `30s`。
/// 同时带上相邻的小单位，避免只显示 `3h`、`1d` 而丢失分钟 / 小时。
fn fmt_relative(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{}d{}h", days, hours)
    } else if hours > 0 {
        format!("{}h{}m", hours, mins)
    } else if mins > 0 {
        format!("{}m", mins)
    } else {
        format!("{}s", secs)
    }
}

/// 渲染某个窗口的额度：返回 `(已用百分比, 距离重置的剩余时间)`。
/// 缺失时百分比为 `n/a`、剩余时间为 `—`。
fn fmt_window(quotas: &[Quota], window: QuotaWindow, now: DateTime<Utc>) -> (String, String) {
    match quotas.iter().find(|q| q.window == window) {
        Some(q) => {
            let pct = match q.usage_ratio() {
                Some(r) => format!("{:.0}%", r * 100.0),
                None => "n/a".to_string(),
            };
            let reset = match q.reset_at {
                Some(t) => fmt_relative(t - now),
                None => "—".to_string(),
            };
            (format!("{:>4}", pct), format!("{:2}", reset))
        }
        None => ("n/a".to_string(), "—".to_string()),
    }
}

/// 选出「当前最值得用」的账号下标。筛选与排序规则：
///
/// - 短窗口（5h）必须仍有剩余，否则不可用、不参与；
/// - 长窗口（7d）已耗尽（used≈100%）的账号排到最后——即便 5h 还有剩余，
///   7d 用光也基本干不了活，不应作为首选；
/// - 其余按 5h 剩余降序、7d 剩余降序、priority 升序、id 升序排序。
///
/// 返回 None 表示没有可用账号。
fn compute_recommend(rows: &[Row]) -> Option<usize> {
    let mut cands: Vec<(bool, f64, f64, i32, String, usize)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let Some(quotas) = row.quotas() else {
            continue;
        };
        let short_rem = quotas
            .iter()
            .find(|q| q.window == QuotaWindow::FiveHour)
            .and_then(|q| q.usage_ratio().map(|r| 1.0 - r))
            .unwrap_or(0.0);
        if short_rem <= 0.0 {
            continue; // 短窗口耗尽，不可用
        }
        let (long_rem, long_exhausted) = quotas
            .iter()
            .find(|q| q.window == QuotaWindow::SevenDay)
            .and_then(|q| q.usage_ratio())
            .map(|r| (1.0 - r, r >= 1.0))
            .unwrap_or((0.0, false));
        cands.push((
            long_exhausted,
            short_rem,
            long_rem,
            row.account.priority,
            row.account.id.0.clone(),
            i,
        ));
    }
    cands.sort_by(|a, b| {
        a.0.cmp(&b.0) // 7d 耗尽的排后面
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)) // 5h 剩余降序
            .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal)) // 7d 剩余降序
            .then(a.3.cmp(&b.3)) // priority 升序
            .then(a.4.cmp(&b.4)) // id 升序
    });
    cands.first().map(|c| c.5)
}

/// 是否所有账号的 7d 窗口都已耗尽（用于底部警告）。
fn all_seven_day_exhausted(rows: &[Row]) -> bool {
    !rows.is_empty()
        && rows.iter().all(|row| match row.quotas() {
            Some(q) => q
                .iter()
                .find(|qq| qq.window == QuotaWindow::SevenDay)
                .and_then(|qq| qq.usage_ratio())
                .map(|r| r >= 1.0)
                .unwrap_or(false),
            None => false,
        })
}

/// 渲染对齐表格（ASCII 表格：`+---+` 边框、列内左右各留一格、`|` 分隔；
/// 按显示宽度对齐，中文 / emoji 账号名也不会把列撑歪）：
/// 编号 / 当前 / 用户名 / 5h / 7d / recommend，底部给结论。
fn render_table(rows: &[Row], recommend_idx: Option<usize>) {
    // 列宽已含两侧内边距（每列 ` content ` 占 width 列）。
    const WIDTHS: [usize; 6] = [4, 5, 36, 14, 14, 12];
    const HEADERS: [&str; 6] = ["#", "ON", "ACCOUNT", "5H", "7D", "RECOMMEND"];

    let now_utc = Utc::now();
    let header_line: String = {
        let mut s = String::from("|");
        for (i, h) in HEADERS.iter().enumerate() {
            s.push_str(&cell(h, WIDTHS[i], Align::Center));
            s.push('|');
        }
        s
    };

    println!("{}", sep_line(&WIDTHS));
    println!("{header_line}");
    println!("{}", sep_line(&WIDTHS));

    for (idx, row) in rows.iter().enumerate() {
        let n = idx + 1;
        let now = if row.account.active { "*" } else { " " };
        let name = row.display_name();
        let name_disp = if name == row.account.id.0 {
            name
        } else {
            format!("{name} ({})", row.account.id.0)
        };
        let err = match &row.quota {
            Some(QuotaOutcome::Failed(e)) => Some(e.as_str()),
            _ => None,
        };
        let (h5, d7, h5_align) = if let Some(e) = err {
            (e.to_string(), "—".to_string(), Align::Left)
        } else {
            let q = row.quotas();
            // 把 `pct%` 与重置剩余时间拼成 `pct% · Xh`，各部分固定列宽，使 `·` 对齐。
            // reset 固定 5 列宽（最大为 `3h15m`），pct 已固定 4 列宽。
            let join = |w: QuotaWindow| -> String {
                match q {
                    Some(qq) => {
                        let (pct, reset) = fmt_window(qq, w, now_utc);
                        format!("{} · {:>5}", pct, reset)
                    }
                    None => "n/a ·   —".to_string(),
                }
            };
            (
                join(QuotaWindow::FiveHour),
                join(QuotaWindow::SevenDay),
                Align::Left,
            )
        };
        let rec = if Some(idx) == recommend_idx { "← use" } else { "" };
        let cells = [
            cell(&n.to_string(), WIDTHS[0], Align::Right),
            cell(now, WIDTHS[1], Align::Center),
            cell(&name_disp, WIDTHS[2], Align::Left),
            cell(&h5, WIDTHS[3], h5_align),
            cell(&d7, WIDTHS[4], Align::Right),
            cell(rec, WIDTHS[5], Align::Left),
        ];
        let mut line = String::from("|");
        for c in cells {
            line.push_str(&c);
            line.push('|');
        }
        println!("{line}");
    }

    println!("{}", sep_line(&WIDTHS));
    println!();
    match recommend_idx {
        Some(i) => {
            let row = &rows[i];
            let seven_gone = row
                .quotas()
                .and_then(|q| q.iter().find(|q| q.window == QuotaWindow::SevenDay))
                .and_then(|q| q.usage_ratio())
                .map(|r| r >= 1.0)
                .unwrap_or(false);
            let note = if seven_gone { " (7d exhausted)" } else { "" };
            println!("recommend: kimi/{} {}{}", row.account.id, row.display_name(), note);
            if all_seven_day_exhausted(rows) {
                println!("⚠ all accounts' 7d quota is exhausted; only 5h remains available");
            }
        }
        None => {
            println!("no account has remaining 5h quota; all are exhausted or unknown");
        }
    }
}

async fn status(ctx: &AppContext) -> Result<()> {
    let rows = gather(ctx).await?;
    if rows.is_empty() {
        println!("No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.");
        return Ok(());
    }
    render_table(&rows, compute_recommend(&rows));
    Ok(())
}

/// `list` 子命令：复用 `gather` 的账号 + 额度 + 资料数据。`--json` 输出机器可读 JSON。
async fn list(ctx: &AppContext, json: bool) -> Result<()> {
    let rows = gather(ctx).await?;
    if rows.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.");
        }
        return Ok(());
    }
    if !json {
        render_table(&rows, compute_recommend(&rows));
        return Ok(());
    }
    let rec_idx = compute_recommend(&rows);
    let items: Vec<serde_json::Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let quotas: Vec<serde_json::Value> = match &row.quota {
                Some(QuotaOutcome::Ready(q)) | Some(QuotaOutcome::Stale(q)) => q
                    .iter()
                    .map(|q| {
                        serde_json::json!({
                            "window": format!("{:?}", q.window),
                            "used": q.used,
                            "limit": q.limit,
                            "usedRatio": q.usage_ratio(),
                            "status": format!("{:?}", q.status),
                            "resetAt": q.reset_at.map(|t| t.to_rfc3339()),
                        })
                    })
                    .collect(),
                _ => Vec::new(),
            };
            serde_json::json!({
                "id": row.account.id.0,
                "label": row.account.label,
                "email": row.profile.as_ref().and_then(|p| p.email.clone()),
                "displayLabel": row.profile.as_ref().and_then(|p| p.display_label.clone()),
                "active": row.account.active,
                "priority": row.account.priority,
                "manualOnly": row.account.manual_only(),
                "routingEnabled": row.account.extra.get(EXTRA_ROUTING_ENABLED).and_then(|v| v.as_bool()),
                "subscriptionExpiresOn": row.account.extra.get(EXTRA_SUBSCRIPTION_EXPIRES_ON).and_then(|v| v.as_str()),
                "recommend": Some(i) == rec_idx,
                "quotas": quotas,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&items)?);
    Ok(())
}

/// 扫本地 `~/.kimi-code`；如果当前激活账号没记录过就 import 进 registry（已存在时只对齐 active）。
/// 用户刚 `rm` 掉的账号有墓碑，跳过。未登录过（文件缺失）静默跳过。
fn sync_local_active(ctx: &AppContext) {
    let removed = AppContext::load_removed().unwrap_or_else(|_| {
        RemovedAccounts::load(&std::path::PathBuf::from(
            "kimi-switch-removed-missing.json",
        ))
    });
    let Ok(id) = ctx.kimi.live_account_id() else {
        return;
    };
    if removed.contains("kimi", &id.0) {
        tracing::debug!(id = %id, "skip tombstoned kimi auto-import");
        return;
    }
    match ctx.kimi.sync_active_metadata(None) {
        Ok(account) => {
            if let Err(e) = ctx.registry.set_active("kimi", &account.id) {
                tracing::debug!(err=%e, "skip kimi active marker");
            }
        }
        Err(e) => tracing::debug!(err=%e, "skip kimi auto-import"),
    }
}

// ---------------------------------------------------------------------------
// login kimi
// ---------------------------------------------------------------------------

fn login(ctx: &AppContext, provider: &str) -> Result<()> {
    match provider {
        "kimi" | "moonshot" => {
            // Kimi 登录是交互式 TUI：约定用户先在 kimi 里登录好，这里只导入当前登录的凭证。
            let account = ctx
                .kimi
                .import_active(None)
                .context("import Kimi login; sign in to Kimi Code first")?;
            ctx.registry
                .set_active("kimi", &account.id)
                .context("mark Kimi login active")?;
            if let Ok(mut removed) = AppContext::load_removed() {
                if let Err(e) = removed.clear("kimi", account.id.0.as_str()) {
                    tracing::warn!(err=%e, "failed to clear removed-account marker");
                }
            }
            ctx.audit
                .append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
            println!("login → kimi/{}", account.id);
            Ok(())
        }
        other => anyhow::bail!("unknown provider: {other} (only `kimi` is supported)"),
    }
}

// ---------------------------------------------------------------------------
// swap
// ---------------------------------------------------------------------------

/// 显式切换激活账号。手动入口，不依赖网络/quota（原子写 + 快照回滚在 provider 内部）。
async fn swap(ctx: &AppContext, id_input: Option<&str>) -> Result<()> {
    let Some(input) = id_input else {
        print_listing(ctx)?;
        return Ok(());
    };

    let acc = resolve_account(ctx, input)?;
    match ctx.kimi.activate(&acc.id).await {
        Ok(()) => {
            ctx.audit
                .append(AuditEvent::ok("activate", "kimi", Some(acc.id.0.as_str())));
            println!("swap → kimi/{}", acc.id);
            Ok(())
        }
        Err(e) => {
            ctx.audit.append(AuditEvent::err(
                "activate",
                "kimi",
                Some(acc.id.0.as_str()),
                &e.to_string(),
            ));
            Err(anyhow::Error::from(e).context(format!("swap kimi/{} failed", acc.id)))
        }
    }
}

/// 无参 `kimi-switch swap`：列出编号 + 用法。**故意不查 quota**，保持「manual swap 不依赖网络」。
fn print_listing(ctx: &AppContext) -> Result<()> {
    let ordered = ctx.list_ordered()?;
    if ordered.is_empty() {
        println!("No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.");
        return Ok(());
    }
    println!("Usage: kimi-switch swap <N | id | kimi/id>");
    println!();
    for (idx, acc) in ordered.iter().enumerate() {
        let n = idx + 1;
        let star = if acc.active { "*" } else { " " };
        println!("  {star} {n:>2}  kimi/{}", acc.id);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

async fn rm(ctx: &AppContext, id_input: &str) -> Result<()> {
    let acc = resolve_account(ctx, id_input)?;

    ctx.registry.remove("kimi", &acc.id)?;
    AppContext::load_removed()?.add("kimi", acc.id.0.as_str())?;

    if let Err(e) = ctx.store.delete("kimi", acc.id.0.as_str(), "blob") {
        tracing::warn!(err=%e, "credential store delete failed (continuing)");
    }
    // 清掉该账号的 quota 缓存，避免尸号数字粘在下一个同 id 导入上。
    let mut cache = QuotaCache::load(&AppPaths::resolve()?.quota_cache_file());
    cache.remove("kimi", &acc.id.0);
    cache.save(&AppPaths::resolve()?.quota_cache_file());

    ctx.audit
        .append(AuditEvent::ok("rm", "kimi", Some(acc.id.0.as_str())));
    println!("removed kimi/{}", acc.id);
    Ok(())
}

// ---------------------------------------------------------------------------
// set（改：更新账号元数据）
// ---------------------------------------------------------------------------

/// 把用户写的订阅到期日归一化为 `YYYY-MM-DD`；空串表示清除该字段。与 GUI 校验保持一致。
fn normalize_subscription_expiry(value: &str) -> Result<Option<String>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("invalid date; use YYYY-MM-DD, e.g. 2026-09-30"))?;
    Ok(Some(date.format("%Y-%m-%d").to_string()))
}

/// 更新账号元数据（CLI 的「改」）。至少传一个字段；字段映射与 GUI `update_account` 一致：
/// label → `account.label`；priority → `account.priority`；routing-enabled → `extra.routing_enabled`；
/// subscription-expires-on → `extra.subscription_expires_on`。
async fn set(
    ctx: &AppContext,
    id_input: &str,
    label: Option<String>,
    priority: Option<i32>,
    routing_enabled: Option<bool>,
    subscription_expires_on: Option<String>,
) -> Result<()> {
    if label.is_none()
        && priority.is_none()
        && routing_enabled.is_none()
        && subscription_expires_on.is_none()
    {
        anyhow::bail!(
            "nothing to update; pass at least one of --label / --priority / \
             --routing-enabled / --subscription-expires-on"
        );
    }

    let acc = resolve_account(ctx, id_input)?;
    let mut account = ctx
        .registry
        .find("kimi", &acc.id)?
        .ok_or_else(|| anyhow::anyhow!("account kimi/{} not found", acc.id))?;

    if let Some(label) = label {
        let label = label.trim();
        if label.is_empty() {
            anyhow::bail!("account label cannot be empty");
        }
        account.label = label.to_string();
    }
    if let Some(priority) = priority {
        if !(-10_000..=10_000).contains(&priority) {
            anyhow::bail!("priority must be between -10000 and 10000");
        }
        account.priority = priority;
    }
    if let Some(enabled) = routing_enabled {
        account
            .extra
            .insert(EXTRA_ROUTING_ENABLED.into(), enabled.into());
    }
    if let Some(expires_on) = subscription_expires_on {
        match normalize_subscription_expiry(&expires_on)? {
            Some(value) => account
                .extra
                .insert(EXTRA_SUBSCRIPTION_EXPIRES_ON.into(), serde_json::Value::String(value)),
            None => account.extra.remove(EXTRA_SUBSCRIPTION_EXPIRES_ON),
        };
    }

    ctx.registry.upsert(account.clone())?;
    ctx.audit
        .append(AuditEvent::ok("update", "kimi", Some(acc.id.0.as_str())));
    println!("updated kimi/{}", acc.id);
    println!(
        "  label={} priority={} routing_enabled={} subscription_expires_on={:?}",
        account.label,
        account.priority,
        account
            .extra
            .get(EXTRA_ROUTING_ENABLED)
            .and_then(|v| v.as_bool())
            .map(|b| b.to_string())
            .unwrap_or_else(|| "unset".into()),
        account
            .extra
            .get(EXTRA_SUBSCRIPTION_EXPIRES_ON)
            .and_then(|v| v.as_str())
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// auto（自动切换：谁还有短窗口额度就切谁）
// ---------------------------------------------------------------------------

/// 自动切换：选 `compute_recommend` 算出的「当前最值得用」的账号激活。
///
/// 选择规则（见 `compute_recommend`）：
/// - 排除短窗口（5h）耗尽的账号（不可用）。
/// - 优先短窗口剩余最多，其次长窗口（7d）剩余最多，再按 priority、id 兜底。
/// - 当前已是该账号则不打扰。
async fn auto(ctx: &AppContext, dry_run: bool) -> Result<()> {
    let rows = gather(ctx).await?;
    if rows.is_empty() {
        anyhow::bail!("no accounts; run `kimi-switch login kimi` first");
    }

    let Some(idx) = compute_recommend(&rows) else {
        anyhow::bail!(
            "no account has remaining short-window (5h) quota; all exhausted or unknown"
        );
    };
    let row = &rows[idx];

    let short_used = row
        .quotas()
        .and_then(|q| q.iter().find(|q| q.window == QuotaWindow::FiveHour))
        .and_then(|q| q.usage_ratio())
        .map(|r| r * 100.0)
        .unwrap_or(0.0);
    let long_used = row
        .quotas()
        .and_then(|q| q.iter().find(|q| q.window == QuotaWindow::SevenDay))
        .and_then(|q| q.usage_ratio())
        .map(|r| r * 100.0)
        .unwrap_or(0.0);

    if row.account.active {
        println!(
            "already on best account kimi/{} (5h used {:.0}% · 7d used {:.0}%)",
            row.account.id, short_used, long_used
        );
        return Ok(());
    }
    if dry_run {
        println!(
            "would swap → kimi/{} (5h used {:.0}% · 7d used {:.0}%)",
            row.account.id, short_used, long_used
        );
        return Ok(());
    }

    let id = row.account.id.clone();
    ctx.kimi.activate(&id).await?;
    ctx.audit
        .append(AuditEvent::ok("auto-activate", "kimi", Some(id.0.as_str())));
    println!(
        "auto swap → kimi/{} (5h used {:.0}% · 7d used {:.0}%)",
        id, short_used, long_used
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// watch（循环监控额度，自动切换后执行 shell 命令）
// ---------------------------------------------------------------------------

/// 清屏并把光标移到左上角（ANSI escape）。这样每次轮询都重绘监控视图，而不是一直往下刷。
fn clear_screen() {
    print!("\x1b[2J\x1b[H");
    let _ = io::stdout().flush();
}

/// 从命令串里尽量抽出被引用的脚本路径，用于可用性检查：
/// - `bash run.sh` / `sh -e run.sh` / `python3 run.py` 等；
/// - 直接 `./run.sh` 或绝对路径 `/x/y.sh`。
///
/// 仅做启发式判断，抽不到返回 None（跳过文件检查）。
fn extract_script_path(cmd: &str) -> Option<PathBuf> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let first = *tokens.first()?;
    if matches!(
        first,
        "bash" | "sh" | "zsh" | "python" | "python3" | "node" | "perl"
    ) {
        for t in &tokens[1..] {
            if t.starts_with('-') {
                continue; // 选项，跳过
            }
            if t.contains('/') && !t.starts_with('>') && !t.starts_with('|') {
                return Some(PathBuf::from(t));
            }
        }
        return None;
    }
    if (first.starts_with("./") || first.starts_with('/')) && first.contains('.') {
        return Some(PathBuf::from(first));
    }
    None
}

/// 执行前检查命令引用的脚本文件是否存在；返回第一个缺失的脚本路径。
/// 用于运行时提示——脚本还没创建时 watch 应继续等待，而不是拒绝启动。
fn missing_script(cmd: &str) -> Option<PathBuf> {
    match extract_script_path(cmd) {
        Some(p) if !p.exists() => Some(p),
        _ => None,
    }
}

/// 执行前校验 shell 命令的语法可用性（不检查脚本文件是否存在）：
/// - 非空；
/// - 用 `bash -n -c` / `sh -n -c` 做语法检查（不实际执行）。
///
/// 脚本文件是否存在放到「每次执行前」再检查（见 `missing_script`），这样即便脚本还没
/// 创建，watch 也能先启动，等文件就绪后再自动执行。
fn validate_shell_command(cmd: &str) -> Result<()> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        anyhow::bail!(
            "command is empty; pass a shell command, e.g. `kimi-switch watch 'bash run.sh'`"
        );
    }

    let output = match ProcCommand::new("bash").args(["-n", "-c", cmd]).output() {
        Ok(o) => o,
        Err(_) => ProcCommand::new("sh")
            .args(["-n", "-c", cmd])
            .output()
            .context("no `bash` or `sh` available to validate the command")?,
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "shell syntax check failed for `{}`:\n{}",
            cmd,
            stderr.trim()
        );
    }
    Ok(())
}

/// watch 日志写入系统临时目录（与 GUI 数据目录完全无关），并限制单文件大小。
const WATCH_LOG_MAX_BYTES: u64 = 1 << 20; // 1 MiB

/// watch 日志路径：系统临时目录下的 `kimi-switch/watch.log`。
/// 即便完全没有安装 GUI app，也能正常创建（只依赖标准临时目录）。
fn watch_log_path() -> PathBuf {
    let dir = std::env::temp_dir().join("kimi-switch");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("watch.log")
}

/// 打开 watch 日志：若已超出大小上限，只保留末尾部分，避免文件无限增长。
fn open_watch_log(path: &Path) -> Result<std::fs::File> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > WATCH_LOG_MAX_BYTES {
            let data = std::fs::read(path)?;
            let keep = &data[data.len().saturating_sub(WATCH_LOG_MAX_BYTES as usize)..];
            std::fs::write(path, keep)?;
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("open watch log file")
}

/// 执行 shell 命令：实时输出同时打到终端与 watch 日志文件，返回退出码。
/// 用 `(command) 2>&1` 把 stderr 并到 stdout，便于统一捕获。
fn run_command_logged(command: &str, log_path: &Path) -> Result<i32> {
    let mut child = ProcCommand::new("sh")
        .arg("-c")
        .arg(format!("({}) 2>&1", command))
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn shell for: {command}"))?;

    let stamp = Utc::now().to_rfc3339();
    let mut log = open_watch_log(log_path)?;
    let _ = writeln!(log, "=== run @ {stamp} :: {command} ===");

    let stdout = child.stdout.take().context("no stdout from child")?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        print!("{line}");
        let _ = io::stdout().flush();
        let _ = log.write_all(line.as_bytes());
        let _ = log.flush();
    }
    let status = child.wait()?;
    let code = status.code().unwrap_or(-1);
    let _ = writeln!(log, "=== exit code {code} ===\n");
    Ok(code)
}

/// watch 主循环：清屏 → 渲染监控表 → 若有账号有 5h 额度就切到它并执行命令；
/// 额度用光则等待（轮询）到有额度再切换后执行。`--cnt` 限制有效执行次数。
async fn watch(ctx: &AppContext, command: String, cnt: Option<usize>, interval: u64) -> Result<()> {
    validate_shell_command(&command)?;

    // watch 日志写到系统临时目录（见 watch_log_path），与 GUI 数据目录完全无关，
    // 且单文件大小受 WATCH_LOG_MAX_BYTES 限制。
    let log_path = watch_log_path();
    let total = cnt.unwrap_or(usize::MAX);
    let mut done = 0usize;
    let mut last_run_at: Option<DateTime<Utc>> = None;
    let mut last_exit: Option<i32> = None;

    loop {
        clear_screen();
        let rows = gather(ctx).await?;
        if rows.is_empty() {
            println!("No accounts. Sign in to Kimi Code, then run `kimi-switch login kimi`.");
            return Ok(());
        }
        let rec_idx = compute_recommend(&rows);
        render_table(&rows, rec_idx);

        println!("▶ watch: {command}");
        let last = match (last_run_at, last_exit) {
            (Some(t), Some(c)) => format!(
                "last: {} · exit {}",
                t.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S"),
                c
            ),
            _ => "last: —".to_string(),
        };
        println!(
            "  runs: {}{} | {} | poll every {}s | log: {} (Ctrl-C to stop)",
            done,
            if total != usize::MAX {
                format!("/{total}")
            } else {
                String::new()
            },
            last,
            interval,
            log_path.display()
        );

        if done >= total {
            println!();
            println!("watch done: completed {done} run(s).");
            break;
        }

        match rec_idx {
            None => {
                println!("  ⏳ no 5h quota available anywhere; waiting for reset…");
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
            Some(idx) => {
                // 执行前再确认脚本已就绪（允许 watch 在脚本尚未创建时就启动）。
                // 脚本缺失时不切账号、不计入次数，只等待其出现后下一轮再执行。
                if let Some(missing) = missing_script(&command) {
                    println!(
                        "  ⏳ script not ready: {} (waiting for it to appear before running)",
                        missing.display()
                    );
                    tokio::time::sleep(Duration::from_secs(interval)).await;
                    continue;
                }

                let target = &rows[idx];
                let active_idx = rows.iter().position(|r| r.account.active);
                if active_idx != Some(idx) {
                    let id = target.account.id.clone();
                    match ctx.kimi.activate(&id).await {
                        Ok(()) => {
                            println!(
                                "  🔁 switched → kimi/{} {}",
                                id,
                                target.display_name()
                            );
                            ctx.audit.append(AuditEvent::ok(
                                "watch-activate",
                                "kimi",
                                Some(id.0.as_str()),
                            ));
                        }
                        Err(e) => {
                            eprintln!("  switch failed: {e}");
                            tokio::time::sleep(Duration::from_secs(interval)).await;
                            continue;
                        }
                    }
                }

                println!("  ▶ run #{}{}: {}", done + 1, if total != usize::MAX { format!(" (of {total})") } else { String::new() }, command);
                println!("  ────────────────────────────────");
                let code = run_command_logged(&command, &log_path)?;
                println!("  ────────────────────────────────");
                println!("  run #{} exited with code {}", done + 1, code);
                last_run_at = Some(Utc::now());
                last_exit = Some(code);
                done += 1;

                // 命令可能已把这台账号的 5h 额度打光，下一轮重新评估 / 切换。
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }
    }
    Ok(())
}
