//! 统一路径解析。所有 Provider 元数据、审计日志、状态文件都从这里取。
//!
//! 遵循 XDG（Linux）/ Library/Application Support（macOS）/ AppData（Windows）。

use crate::error::{Error, Result};
use directories::ProjectDirs;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// 账号 ID 到路由器私有目录名的稳定映射，避免在路径中暴露原始 ID。
pub fn router_account_dir_name(account_id: &str) -> String {
    let digest = Sha256::digest(account_id.as_bytes());
    let mut out = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// ACP 目标标识是否合法。目标名会进入本机路径，只允许安全的短 ASCII 标识。
pub fn valid_router_target(target: &str) -> bool {
    let bytes = target.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes.first().is_some_and(u8::is_ascii_lowercase)
            && !bytes.first().is_some_and(u8::is_ascii_digit)
        || !bytes.last().is_some_and(u8::is_ascii_lowercase)
            && !bytes.last().is_some_and(u8::is_ascii_digit)
        || !bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return false;
    }
    !matches!(
        target,
        "con"
            | "prn"
            | "aux"
            | "nul"
            | "com1"
            | "com2"
            | "com3"
            | "com4"
            | "com5"
            | "com6"
            | "com7"
            | "com8"
            | "com9"
            | "lpt1"
            | "lpt2"
            | "lpt3"
            | "lpt4"
            | "lpt5"
            | "lpt6"
            | "lpt7"
            | "lpt8"
            | "lpt9"
    )
}

/// 从旧版本数据目录一次性迁移（老用户的账号库不丢）。
fn migrate_legacy_dirs(new_dirs: &ProjectDirs) {
    let Some(old) = ProjectDirs::from("dev", "kimi-switch", "kimi-switch") else {
        return;
    };
    for (old_dir, new_dir) in [
        (old.config_dir(), new_dirs.config_dir()),
        (old.data_dir(), new_dirs.data_dir()),
        (old.cache_dir(), new_dirs.cache_dir()),
    ] {
        if old_dir.exists() && !new_dir.exists() {
            // Windows 下 rename 要求目标父目录已存在，先补齐。
            if let Some(parent) = new_dir.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(old_dir, new_dir);
        }
    }
}

/// 项目维度的标准路径集合。
#[derive(Debug, Clone)]
pub struct AppPaths {
    /// 配置：registry.toml、provider 元数据。
    pub config_dir: PathBuf,
    /// 数据：审计日志、备份快照。
    pub data_dir: PathBuf,
    /// 运行时状态：当前激活账号缓存、daemon pid 等。
    pub state_dir: PathBuf,
    /// 缓存：额度查询缓存等可丢弃数据。
    pub cache_dir: PathBuf,
}

impl AppPaths {
    /// 解析默认路径；目录不存在时会自动创建。
    ///
    /// 设置 `KIMI_SWITCH_HOME` 时，配置、数据、状态与缓存会全部收口到该绝对路径下。
    /// 该入口用于跨平台隔离运行与测试，避免 Windows 系统目录无法由 XDG 变量重定向。
    pub fn resolve() -> Result<Self> {
        let (config_dir, data_dir, cache_dir) = match std::env::var_os("KIMI_SWITCH_HOME") {
            Some(root) => {
                let root = PathBuf::from(root);
                if !root.is_absolute() {
                    return Err(Error::Config(
                        "KIMI_SWITCH_HOME must be an absolute path".into(),
                    ));
                }
                (root.join("config"), root.join("data"), root.join("cache"))
            }
            None => {
                let dirs = ProjectDirs::from("dev", "kimi-switch", "kimi-switch")
                    .ok_or_else(|| Error::Config("cannot resolve user directories".into()))?;
                migrate_legacy_dirs(&dirs);
                (
                    dirs.config_dir().to_path_buf(),
                    dirs.data_dir().to_path_buf(),
                    dirs.cache_dir().to_path_buf(),
                )
            }
        };
        // ProjectDirs 没有 state_dir 抽象，按平台约定挂在 data_dir 下。
        let state_dir = data_dir.join("state");

        for d in [&config_dir, &data_dir, &state_dir, &cache_dir] {
            std::fs::create_dir_all(d)?;
        }

        Ok(Self {
            config_dir,
            data_dir,
            state_dir,
            cache_dir,
        })
    }

    /// 账号注册表路径：`<config_dir>/registry.toml`。
    pub fn registry_file(&self) -> PathBuf {
        self.config_dir.join("registry.toml")
    }

    /// 数值调优配置文件路径：`<config_dir>/config.toml`。
    ///
    /// 文件可缺失：缺则使用 [`crate::defaults`] 中的编译期默认值。详见 [`crate::settings`]。
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// ACP 目标与账号池配置：`<config_dir>/acp-targets.toml`。
    pub fn acp_config_file(&self) -> PathBuf {
        self.config_dir.join("acp-targets.toml")
    }

    /// 明文凭证文件：`<data_dir>/credentials.json`（[`crate::store::FileStore`] 后端，`0600`）。
    /// 放 data 而非 config，避免被随 config 一起同步出去。
    pub fn credentials_file(&self) -> PathBuf {
        self.data_dir.join("credentials.json")
    }

    /// 审计日志：`<data_dir>/audit.log`。
    pub fn audit_log(&self) -> PathBuf {
        self.data_dir.join("audit.log")
    }

    /// 切换前快照根目录：`<state_dir>/snapshots/`。
    pub fn snapshots_dir(&self) -> PathBuf {
        self.state_dir.join("snapshots")
    }

    /// kimi-switchd 守护进程 PID 文件:`<state_dir>/kimi-switchd.pid`。
    /// 通过 fs2 文件锁标识唯一存活实例;退出后保留 PID 仅作信息参考。
    pub fn daemon_pid_file(&self) -> PathBuf {
        self.state_dir.join("kimi-switchd.pid")
    }

    /// kimi-switchd 守护进程日志文件:`<data_dir>/kimi-switchd.log`。
    /// 用 append 模式打开,后续可由 logrotate 切割。
    pub fn daemon_log_file(&self) -> PathBuf {
        self.data_dir.join("kimi-switchd.log")
    }

    /// 本地控制服务随机令牌；文件必须仅当前用户可读。
    pub fn control_token_file(&self) -> PathBuf {
        self.state_dir.join("control-token")
    }

    /// 本地控制服务当前监听地址，不包含认证令牌。
    pub fn control_endpoint_file(&self) -> PathBuf {
        self.state_dir.join("control-endpoint.json")
    }

    /// ACP 路由器的非敏感会话归属状态。
    pub fn router_state_file(&self) -> PathBuf {
        self.router_state_file_for_target("default")
    }

    /// 指定 ACP 目标的非敏感会话归属状态。
    pub fn router_state_file_for_target(&self, target: &str) -> PathBuf {
        if target == "default" {
            self.state_dir.join("router-state.json")
        } else {
            self.state_dir
                .join("router-targets")
                .join(target)
                .join("router-state.json")
        }
    }

    /// ACP 路由器的账号隔离目录与共享会话目录。
    pub fn router_data_dir(&self) -> PathBuf {
        self.router_data_dir_for_target("default")
    }

    /// 指定 ACP 目标的账号隔离目录与共享会话目录。
    pub fn router_data_dir_for_target(&self, target: &str) -> PathBuf {
        if target == "default" {
            self.data_dir.join("router")
        } else {
            self.data_dir.join("router-targets").join(target)
        }
    }

    /// 指定账号的路由器隔离 Kimi home。
    pub fn router_account_home(&self, account_id: &str) -> PathBuf {
        self.router_account_home_for_target("default", account_id)
    }

    /// 指定 ACP 目标和账号的隔离 Kimi home。
    pub fn router_account_home_for_target(&self, target: &str, account_id: &str) -> PathBuf {
        self.router_data_dir_for_target(target)
            .join("accounts")
            .join(router_account_dir_name(account_id))
            .join("kimi-home")
    }

    /// ACP 路由器单实例锁。
    pub fn router_lock_file(&self) -> PathBuf {
        self.router_lock_file_for_target("default")
    }

    /// 指定 ACP 目标的单实例锁。
    pub fn router_lock_file_for_target(&self, target: &str) -> PathBuf {
        if target == "default" {
            self.state_dir.join("router.lock")
        } else {
            self.state_dir
                .join("router-targets")
                .join(target)
                .join("router.lock")
        }
    }

    /// 账号级 ACP 租约锁，防止同一账号同时进入两个目标进程。
    pub fn router_account_lock_file(&self, account_id: &str) -> PathBuf {
        self.state_dir
            .join("router-account-locks")
            .join(format!("{}.lock", router_account_dir_name(account_id)))
    }

    /// quota 查询结果缓存：`<cache_dir>/quota_cache.json`。
    pub fn quota_cache_file(&self) -> PathBuf {
        self.cache_dir.join("quota_cache.json")
    }

    /// 用户显式删除过的账号墓碑：`<config_dir>/removed.json`。
    /// 默认入口自动导入会跳过这些 id，直到 `kimi-switch login` 再导入。
    pub fn removed_file(&self) -> PathBuf {
        self.config_dir.join("removed.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &std::path::Path) -> AppPaths {
        AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
        }
    }

    #[test]
    fn validates_router_target_path_segments() {
        assert!(valid_router_target("vscode-fork"));
        assert!(valid_router_target("zed.work_2"));
        assert!(!valid_router_target(""));
        assert!(!valid_router_target("../escape"));
        assert!(!valid_router_target("with space"));
        assert!(!valid_router_target("VSCode"));
        assert!(!valid_router_target("target."));
        assert!(!valid_router_target("con"));
    }

    #[test]
    fn default_target_keeps_legacy_paths() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        assert_eq!(
            paths.router_state_file_for_target("default"),
            paths.state_dir.join("router-state.json")
        );
        assert_eq!(
            paths.router_data_dir_for_target("default"),
            paths.data_dir.join("router")
        );
    }

    #[test]
    fn named_targets_have_isolated_state_and_data() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        let vscode_state = paths.router_state_file_for_target("vscode-fork");
        let zed_state = paths.router_state_file_for_target("zed");
        assert_ne!(vscode_state, zed_state);
        assert!(vscode_state.ends_with("router-targets/vscode-fork/router-state.json"));
        assert!(paths
            .router_account_home_for_target("vscode-fork", "account-a")
            .starts_with(paths.data_dir.join("router-targets/vscode-fork")));
    }
}
