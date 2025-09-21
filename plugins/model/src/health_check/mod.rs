//! # 健康检查模块
//! 
//! 提供系统健康监控功能，包括：
//! - 记忆使用情况监控
//! - 文件大小检查
//! - 系统状态报告
//! - 警告和错误检测

use crate::memory::MemoryManager;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use kovi::tokio::time::sleep;

/// 健康状态结构体
/// 
/// 包含系统的整体健康状态信息
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HealthStatus {
    /// 系统是否健康
    pub is_healthy: bool,
    /// 内存使用情况
    pub memory_usage: MemoryUsage,
    /// 最后检查时间
    pub last_check: chrono::DateTime<Local>,
    /// 错误列表
    pub errors: Vec<String>,
    /// 警告列表
    pub warnings: Vec<String>,
}

/// 内存使用情况结构体
/// 
/// 记录各种类型记忆的使用情况
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MemoryUsage {
    /// 总记忆数量
    pub total_memories: usize,
    /// 用户档案数量
    pub user_profiles: usize,
    /// 群组档案数量
    pub group_profiles: usize,
    /// 记忆文件大小（字节）
    pub memory_file_size: u64,
}

pub struct HealthChecker {
    memory_manager: Arc<MemoryManager>,
    last_health_status: Option<HealthStatus>,
}

impl HealthChecker {
    pub fn new(memory_manager: Arc<MemoryManager>) -> Self {
        Self {
            memory_manager,
            last_health_status: None,
        }
    }

    pub async fn check_health(&mut self) -> HealthStatus {
        let errors = Vec::new();
        let mut warnings = Vec::new();

        // 检查记忆管理器
        let memory_usage = self.check_memory_usage().await;
        
        // 检查记忆文件大小
        if memory_usage.memory_file_size > 10 * 1024 * 1024 { // 10MB
            warnings.push("记忆文件过大，建议清理".to_string());
        }

        // 检查记忆数量
        if memory_usage.total_memories > 5000 {
            warnings.push("记忆数量过多，可能影响性能".to_string());
        }

        // 检查用户档案数量
        if memory_usage.user_profiles > 1000 {
            warnings.push("用户档案数量过多".to_string());
        }

        let is_healthy = errors.is_empty();

        let status = HealthStatus {
            is_healthy,
            memory_usage,
            last_check: Local::now(),
            errors,
            warnings,
        };

        self.last_health_status = Some(status.clone());
        status
    }

    async fn check_memory_usage(&self) -> MemoryUsage {
        let memories = self.memory_manager.get_recent_memories(0).await;
        let user_profiles = self.memory_manager.get_all_user_profiles().await;
        let group_profiles = self.memory_manager.get_all_group_profiles().await;
        
        let memory_file_size = std::fs::metadata("bot_memory.json")
            .map(|m| m.len())
            .unwrap_or(0);

        MemoryUsage {
            total_memories: memories.len(),
            user_profiles: user_profiles.len(),
            group_profiles: group_profiles.len(),
            memory_file_size,
        }
    }

    pub async fn start_health_monitoring(&mut self) {
        loop {
            let health_status = self.check_health().await;
            
            if !health_status.is_healthy {
                eprintln!("[HEALTH] 系统健康检查发现问题:");
                for error in &health_status.errors {
                    eprintln!("[HEALTH] 错误: {}", error);
                }
            }

            if !health_status.warnings.is_empty() {
                println!("[HEALTH] 系统警告:");
                for warning in &health_status.warnings {
                    println!("[HEALTH] 警告: {}", warning);
                }
            }

            if health_status.is_healthy && health_status.warnings.is_empty() {
                println!("[HEALTH] 系统运行正常");
            }

            // 每5分钟检查一次
            sleep(Duration::from_secs(300)).await;
        }
    }

    pub fn get_last_health_status(&self) -> Option<&HealthStatus> {
        self.last_health_status.as_ref()
    }
}
