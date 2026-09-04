use crate::error::{OpenMcpGdbError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub gdb_path: PathBuf,
    #[serde(default)]
    pub gdb_options: String,
    pub codebase_dir: PathBuf,
    pub executable_path: PathBuf,
    #[serde(default = "default_server_name")]
    pub mcp_server_name: String,
    #[serde(default = "default_server_url")]
    pub mcp_server_url: String,
    #[serde(default = "default_lines_before")]
    pub display_lines_before_current: usize,
    #[serde(default = "default_lines_after")]
    pub display_lines_after_current: usize,
    #[serde(default = "default_backtrace")]
    pub display_backtrace: usize,
    #[serde(default = "default_variable_list")]
    pub display_variable_list: usize,
    #[serde(default)]
    pub display_join_current_code: bool,
    /// Optional "host:port" for OpenOCD's telnet control interface. Used as a
    /// fallback interrupt mechanism (send "halt") on platforms without POSIX
    /// signals (Windows), since the running gdb process can't be sent SIGINT there.
    #[serde(default)]
    pub openocd_telnet_addr: Option<String>,
}

fn default_server_name() -> String {
    "MCP GDB Server".to_string()
}

fn default_server_url() -> String {
    "https://localhost:9443".to_string()
}

fn default_lines_before() -> usize {
    7
}

fn default_lines_after() -> usize {
    8
}

fn default_backtrace() -> usize {
    6
}

fn default_variable_list() -> usize {
    9
}

impl ServerConfig {
    pub fn from_file(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path).map_err(OpenMcpGdbError::Io)?;
        let config: Self = serde_json::from_str(&data).map_err(OpenMcpGdbError::Json)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.gdb_path.is_absolute() {
            return Err(OpenMcpGdbError::InvalidConfig(
                "gdb_path must be absolute".to_string(),
            ));
        }
        if !self.codebase_dir.is_absolute() {
            return Err(OpenMcpGdbError::InvalidConfig(
                "codebase_dir must be absolute".to_string(),
            ));
        }
        if !self.executable_path.is_absolute() {
            return Err(OpenMcpGdbError::InvalidConfig(
                "executable_path must be absolute".to_string(),
            ));
        }
        if self.display_backtrace == 0 {
            return Err(OpenMcpGdbError::InvalidConfig(
                "display_backtrace must be > 0".to_string(),
            ));
        }
        if self.display_variable_list == 0 {
            return Err(OpenMcpGdbError::InvalidConfig(
                "display_variable_list must be > 0".to_string(),
            ));
        }
        Ok(())
    }
}
