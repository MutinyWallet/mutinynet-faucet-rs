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
            self.add_payment_impl(format!("github:{}", user.username).as_str(), amount)
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

    // Get the total amount of payments for the given ip
    pub async fn get_total_payments(&self, ip: &str) -> u64 {
        let mut trackers = self.trackers.lock().await;
        match trackers.get_mut(ip) {
            Some(tracker) => tracker.sum_payments(),
            None => 0,
        }
    }

    pub async fn verify_payments(
        &self,
        ip: &str,
        address: Option<&Address>,
        user: Option<&AuthUser>,
    ) -> bool {
        let mut total = 0;
        let mut addr_amt = 0;
        let mut trackers = self.trackers.lock().await;
        if let Some(tracker) = trackers.get_mut(ip) {
            total += tracker.sum_payments();
        }
        if let Some(address) = address {
            if let Some(tracker) = trackers.get_mut(&address.to_string()) {
                let amt = tracker.sum_payments();
                total += amt;
                addr_amt = amt;
            }
        };
        if let Some(user) = user {
            if let Some(tracker) = trackers.get_mut(format!("github:{}", user.username).as_str()) {
                total += tracker.sum_payments();
            }
        }
        total >= MAX_SEND_AMOUNT * 10 || addr_amt >= MAX_SEND_AMOUNT
    }
}
