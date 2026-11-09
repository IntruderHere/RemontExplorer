//! Проверка доступности сетевых ресурсов (DNS + TCP).
//!
//! Только DNS-резолвинг ЛЖЁТ про выключенную шару: имя резолвится, но SMB
//! не отвечает. Поэтому: DNS + TCP-подключение к порту 445 с таймаутом.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const SMB_PORT: u16 = 445;
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    Reachable,
    DnsFailed,
    Unreachable,
}

/// Проверяет корень `\\server\share`: DNS + TCP:445.
pub fn check_server(server: &str) -> CheckResult {
    let host = server.trim_start_matches(['\\', '/']);
    if host.is_empty() {
        return CheckResult::Unreachable;
    }
    let addrs = match (host, SMB_PORT).to_socket_addrs() {
        Ok(a) => a.collect::<Vec<_>>(),
        Err(_) => return CheckResult::DnsFailed,
    };
    if addrs.is_empty() {
        return CheckResult::DnsFailed;
    }
    for addr in addrs {
        if TcpStream::connect_timeout(&addr, TIMEOUT).is_ok() {
            return CheckResult::Reachable;
        }
    }
    CheckResult::Unreachable
}
