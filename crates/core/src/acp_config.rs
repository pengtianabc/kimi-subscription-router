//! ACP 目标与账号池配置。
//!
//! 配置只保存 target 名和账号 ID，不保存任何凭证。路由器在没有收到显式
//! `--account` 时读取这里的配置，从而让 GUI 管理多个 ACP 客户端的账号池。

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths::{valid_router_target, AppPaths};
use crate::private_fs::restrict_file;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AcpConfig {
    /// 保留给官方 Kimi CLI 的账号；所有 App 管理的 ACP 目标都会排除它们。
    #[serde(default)]
    pub cli_reserved_accounts: Vec<String>,
    /// 每个 ACP 客户端一个独立目标。
    #[serde(default)]
    pub targets: Vec<AcpTargetConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpTargetConfig {
    /// 会进入本机路径和路由器锁，只允许安全的 target 标识。
    pub target: String,
    /// 为空时表示该目标使用所有已开启路由的账号。
    #[serde(default)]
    pub accounts: Vec<String>,
}

impl AcpConfig {
    /// 使用默认路径加载配置；文件不存在时返回空配置。
    pub fn load_default() -> Result<Self> {
        let paths = AppPaths::resolve()?;
        Self::load(&paths.acp_config_file())
    }

    /// 从指定路径加载并校验配置。
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("read {}: {e}", path.display())))?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let config: Self = toml::from_str(&raw)
            .map_err(|e| Error::Config(format!("parse {}: {e}", path.display())))?;
        config.validate()?;
        Ok(config)
    }

    /// 保存到默认路径。
    pub fn save_default(&self) -> Result<()> {
        let paths = AppPaths::resolve()?;
        self.save(&paths.acp_config_file())
    }

    /// 原子保存配置并限制文件权限。
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let serialized = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("serialize {}: {e}", path.display())))?;
        let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serialized)
            .map_err(|e| Error::Config(format!("write {}: {e}", tmp.display())))?;
        restrict_file(&tmp)
            .map_err(|e| Error::Config(format!("restrict {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| Error::Config(format!("replace {}: {e}", path.display())))?;
        restrict_file(path)
            .map_err(|e| Error::Config(format!("restrict {}: {e}", path.display())))?;
        Ok(())
    }

    /// 查找指定目标的账号池。
    pub fn target(&self, target: &str) -> Option<&AcpTargetConfig> {
        self.targets.iter().find(|entry| entry.target == target)
    }

    fn validate(&self) -> Result<()> {
        let mut targets = HashSet::new();
        let mut assigned_accounts = HashSet::new();
        for account in &self.cli_reserved_accounts {
            validate_account_id(account, "Kimi CLI reserved pool")?;
            if !assigned_accounts.insert(account) {
                return Err(Error::Config(format!(
                    "duplicate account {:?} in Kimi CLI reserved pool",
                    account
                )));
            }
        }
        if self.targets.len() > 1 && self.targets.iter().any(|entry| entry.accounts.is_empty()) {
            return Err(Error::Config(
                "multiple ACP targets require explicit, non-overlapping account pools".into(),
            ));
        }
        for entry in &self.targets {
            if !valid_router_target(&entry.target) {
                return Err(Error::Config(format!(
                    "invalid ACP target {:?}; use 1-64 lowercase ASCII letters, digits, '.', '_' or '-'",
                    entry.target
                )));
            }
            if !targets.insert(&entry.target) {
                return Err(Error::Config(format!(
                    "duplicate ACP target {:?}",
                    entry.target
                )));
            }
            let mut accounts = HashSet::new();
            for account in &entry.accounts {
                validate_account_id(account, &format!("ACP target {:?}", entry.target))?;
                if !accounts.insert(account) {
                    return Err(Error::Config(format!(
                        "duplicate account {:?} in ACP target {:?}",
                        account, entry.target
                    )));
                }
                if !assigned_accounts.insert(account) {
                    return Err(Error::Config(format!(
                        "account {:?} is assigned to more than one Kimi CLI/ACP pool",
                        account
                    )));
                }
            }
        }
        Ok(())
    }
}

fn validate_account_id(account: &str, owner: &str) -> Result<()> {
    if account.trim().is_empty() || account.contains(['\r', '\n']) {
        return Err(Error::Config(format!("invalid account ID in {owner}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            AcpConfig::load(&tmp.path().join("acp.toml")).unwrap(),
            AcpConfig::default()
        );
    }

    #[test]
    fn save_round_trips_target_pools() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("acp.toml");
        let config = AcpConfig {
            cli_reserved_accounts: vec!["account-cli".into()],
            targets: vec![AcpTargetConfig {
                target: "kimi-vscode-fork".into(),
                accounts: vec!["account-a".into(), "account-b".into()],
            }],
        };
        config.save(&path).unwrap();
        assert_eq!(AcpConfig::load(&path).unwrap(), config);
    }

    #[test]
    fn rejects_unsafe_or_duplicate_targets() {
        let config = AcpConfig {
            cli_reserved_accounts: Vec::new(),
            targets: vec![
                AcpTargetConfig {
                    target: "bad target".into(),
                    accounts: Vec::new(),
                },
                AcpTargetConfig {
                    target: "bad target".into(),
                    accounts: Vec::new(),
                },
            ],
        };
        let tmp = tempfile::tempdir().unwrap();
        assert!(config.save(&tmp.path().join("acp.toml")).is_err());
    }

    #[test]
    fn multiple_targets_require_disjoint_explicit_pools() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("acp.toml");
        let overlapping = AcpConfig {
            cli_reserved_accounts: Vec::new(),
            targets: vec![
                AcpTargetConfig {
                    target: "vscode".into(),
                    accounts: vec!["shared".into()],
                },
                AcpTargetConfig {
                    target: "zed".into(),
                    accounts: vec!["shared".into()],
                },
            ],
        };
        assert!(overlapping.save(&path).is_err());

        let implicit_all = AcpConfig {
            cli_reserved_accounts: Vec::new(),
            targets: vec![
                AcpTargetConfig {
                    target: "vscode".into(),
                    accounts: Vec::new(),
                },
                AcpTargetConfig {
                    target: "zed".into(),
                    accounts: vec!["account-b".into()],
                },
            ],
        };
        assert!(implicit_all.save(&path).is_err());
    }

    #[test]
    fn cli_reserved_accounts_cannot_overlap_acp_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("acp.toml");
        let config = AcpConfig {
            cli_reserved_accounts: vec!["account-cli".into()],
            targets: vec![AcpTargetConfig {
                target: "vscode".into(),
                accounts: vec!["account-cli".into()],
            }],
        };

        assert!(config.save(&path).is_err());
    }
}
