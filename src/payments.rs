use crate::auth::AuthUser;
use crate::MAX_SEND_AMOUNT;
use bitcoin::Address;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const CACHE_DURATION: Duration = Duration::from_secs(86_400); // 1 day

struct Payment {
    time: Instant,
    amount: u64,
}

struct PaymentTracker {
    payments: VecDeque<Payment>,
}

impl PaymentTracker {
    pub fn new() -> Self {
        PaymentTracker {
            payments: VecDeque::new(),
        }
    }

    pub fn add_payment(&mut self, amount: u64) {
        let now = Instant::now();
        let payment = Payment { time: now, amount };

        self.payments.push_back(payment);
    }

    fn clean_old_payments(&mut self) {
        let now = Instant::now();
        while let Some(payment) = self.payments.front() {
            if now.duration_since(payment.time) < CACHE_DURATION {
                break;
            }

            self.payments.pop_front();
        }
    }

    pub fn sum_payments(&mut self) -> u64 {
        self.clean_old_payments();
        self.payments.iter().map(|p| p.amount).sum()
    }
}

#[derive(Clone)]
pub struct PaymentsByIp {
    trackers: Arc<Mutex<HashMap<String, PaymentTracker>>>,
}

impl PaymentsByIp {
    pub fn new() -> Self {
        PaymentsByIp {
            trackers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn add_payment(
        &self,
        ip: &str,
        address: Option<&Address>,
        user: Option<&AuthUser>,
        amount: u64,
    ) {
        self.add_payment_impl(ip, amount).await;
        if let Some(address) = address {
            self.add_payment_impl(&address.to_string(), amount).await;
        }
        if let Some(user) = user {
            self.add_payment_impl(format!("user:{}", user.username).as_str(), amount)
                .await;
        }
    }

    // Add a payment to the tracker for the given ip
    async fn add_payment_impl(&self, ip: &str, amount: u64) {
        let mut trackers = self.trackers.lock().await;
        let tracker = trackers
            .entry(ip.to_string())
            .or_insert_with(PaymentTracker::new);
        tracker.add_payment(amount);
    }

    /// Get rolling-24h usage for an IP and (optionally) a user, in a single lock acquisition.
    pub async fn get_usage(&self, ip: &str, user: Option<&AuthUser>) -> (u64, u64) {
        let mut trackers = self.trackers.lock().await;
        let ip_amt = trackers.get_mut(ip).map(|t| t.sum_payments()).unwrap_or(0);
        let user_amt = user
            .and_then(|u| trackers.get_mut(format!("user:{}", u.username).as_str()))
            .map(|t| t.sum_payments())
            .unwrap_or(0);
        (ip_amt, user_amt)
    }

    /// Atomically check the rolling-24h total for each (key, max) pair and,
    /// if none would exceed its limit, record the payment against all keys.
    /// Returns false without recording anything when any key would exceed.
    pub async fn try_reserve(&self, keys: &[(&str, u64)], amount: u64) -> bool {
        let mut trackers = self.trackers.lock().await;
        for (key, max) in keys {
            if let Some(tracker) = trackers.get_mut(*key) {
                if tracker.sum_payments() + amount > *max {
                    return false;
                }
            }
        }
        for (key, _) in keys {
            trackers
                .entry(key.to_string())
                .or_insert_with(PaymentTracker::new)
                .add_payment(amount);
        }
        true
    }

    /// Release a prior reservation after the corresponding external payment
    /// failed. Each key removes one matching entry, mirroring `try_reserve`.
    pub async fn release(&self, keys: &[(&str, u64)], amount: u64) {
        let mut trackers = self.trackers.lock().await;
        for (key, _) in keys {
            if let Some(tracker) = trackers.get_mut(*key) {
                if let Some(position) = tracker
                    .payments
                    .iter()
                    .rposition(|payment| payment.amount == amount)
                {
                    tracker.payments.remove(position);
                }
            }
        }
    }

    /// Atomically check the standard per-IP/address/user limits and record
    /// the payment. Returns false without recording when over the limit.
    pub async fn try_reserve_payment(
        &self,
        ip: &str,
        address: Option<&Address>,
        user: Option<&AuthUser>,
        amount: u64,
    ) -> bool {
        let addr_key;
        let user_key;
        let mut keys: Vec<(&str, u64)> = vec![(ip, MAX_SEND_AMOUNT)];
        if let Some(address) = address {
            addr_key = address.to_string();
            keys.push((&addr_key, MAX_SEND_AMOUNT));
        }
        if let Some(user) = user {
            user_key = format!("user:{}", user.username);
            keys.push((&user_key, MAX_SEND_AMOUNT));
        }
        self.try_reserve(&keys, amount).await
    }

    /// Release a standard payment reservation after the external operation
    /// failed before producing its side effect.
    pub async fn release_payment(
        &self,
        ip: &str,
        address: Option<&Address>,
        user: Option<&AuthUser>,
        amount: u64,
    ) {
        let addr_key;
        let user_key;
        let mut keys: Vec<(&str, u64)> = vec![(ip, MAX_SEND_AMOUNT)];
        if let Some(address) = address {
            addr_key = address.to_string();
            keys.push((&addr_key, MAX_SEND_AMOUNT));
        }
        if let Some(user) = user {
            user_key = format!("user:{}", user.username);
            keys.push((&user_key, MAX_SEND_AMOUNT));
        }
        self.release(&keys, amount).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_reservation_can_be_released() {
        let payments = PaymentsByIp::new();
        let keys = [("user", 100), ("global", 100)];

        assert!(payments.try_reserve(&keys, 100).await);
        assert!(!payments.try_reserve(&keys, 1).await);

        payments.release(&keys, 100).await;
        assert!(payments.try_reserve(&keys, 100).await);
    }
}
