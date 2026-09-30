use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant},
};

const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(300);
const LOCKOUT: Duration = Duration::from_secs(60);
const MAX_TRACKED_ADDRESSES: usize = 4096;

struct Failures {
    count: u32,
    first: Instant,
    locked_until: Option<Instant>,
}

/// Throttles password guessing per client address. The fixed delay on a wrong
/// password does not stop parallel requests, so repeated failures lock the
/// address out for a while.
#[derive(Default)]
pub struct LoginLimiter {
    clients: HashMap<IpAddr, Failures>,
}

impl LoginLimiter {
    /// Returns how long the caller must wait if this address is locked out.
    pub fn check(&self, address: IpAddr, now: Instant) -> Option<Duration> {
        let until = self.clients.get(&address)?.locked_until?;
        until.checked_duration_since(now).filter(|d| !d.is_zero())
    }

    pub fn record_failure(&mut self, address: IpAddr, now: Instant) {
        if self.clients.len() >= MAX_TRACKED_ADDRESSES {
            self.clients.retain(|_, f| Self::active(f, now));
            if self.clients.len() >= MAX_TRACKED_ADDRESSES {
                self.clients.clear();
            }
        }
        let entry = self.clients.entry(address).or_insert(Failures {
            count: 0,
            first: now,
            locked_until: None,
        });
        if !Self::active(entry, now) {
            *entry = Failures {
                count: 0,
                first: now,
                locked_until: None,
            };
        }
        entry.count += 1;
        if entry.count >= MAX_FAILURES {
            entry.locked_until = Some(now + LOCKOUT);
            entry.count = 0;
            entry.first = now;
        }
    }

    pub fn record_success(&mut self, address: IpAddr) {
        self.clients.remove(&address);
    }

    fn active(failures: &Failures, now: Instant) -> bool {
        failures.locked_until.is_some_and(|until| until > now)
            || now.duration_since(failures.first) < FAILURE_WINDOW
    }
}

/// Removes expired sessions so abandoned logins don't accumulate forever.
pub fn prune_sessions(sessions: &mut HashMap<String, Instant>, now: Instant) {
    sessions.retain(|_, expiry| *expiry > now);
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2));

    #[test]
    fn locks_out_after_repeated_failures() {
        let mut limiter = LoginLimiter::default();
        let now = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            limiter.record_failure(IP, now);
            assert!(limiter.check(IP, now).is_none());
        }
        limiter.record_failure(IP, now);
        assert!(limiter.check(IP, now).is_some());
        assert!(limiter.check(IP, now + LOCKOUT + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn lockout_is_per_address_and_cleared_by_success() {
        let mut limiter = LoginLimiter::default();
        let now = Instant::now();
        let other = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 3));
        for _ in 0..MAX_FAILURES {
            limiter.record_failure(IP, now);
        }
        assert!(limiter.check(other, now).is_none());
        limiter.record_success(IP);
        assert!(limiter.check(IP, now).is_none());
    }

    #[test]
    fn old_failures_expire() {
        let mut limiter = LoginLimiter::default();
        let now = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            limiter.record_failure(IP, now);
        }
        let later = now + FAILURE_WINDOW + Duration::from_secs(1);
        limiter.record_failure(IP, later);
        assert!(limiter.check(IP, later).is_none());
    }

    #[test]
    fn prunes_expired_sessions() {
        let now = Instant::now();
        let mut sessions = HashMap::from([
            ("old".to_string(), now - Duration::from_secs(1)),
            ("new".to_string(), now + Duration::from_secs(60)),
        ]);
        prune_sessions(&mut sessions, now);
        assert_eq!(sessions.keys().collect::<Vec<_>>(), ["new"]);
    }
}
