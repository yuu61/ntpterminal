use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;

use application::port::NonceSource;
use domain::shared_kernel::NtpTimestamp;

/// 予測しにくい 64 bit のノンスを作る。
///
/// 要求の Transmit Timestamp に入れ、応答の Originate Timestamp と照合して
/// 「自分の問い合わせへの応答か」を確かめる（遅れて届いた応答や偽の応答を弾く）。
/// 暗号用途ではないので、プロセスごとにランダムな鍵を持つ std の SipHash で十分。
#[derive(Clone, Debug)]
pub struct RandomNonce {
    hasher: RandomState,
    counter: u64,
}

impl RandomNonce {
    pub fn new() -> Self {
        Self {
            hasher: RandomState::new(),
            counter: 0,
        }
    }
}

impl Default for RandomNonce {
    fn default() -> Self {
        Self::new()
    }
}

impl NonceSource for RandomNonce {
    fn next_nonce(&mut self) -> NtpTimestamp {
        loop {
            self.counter = self.counter.wrapping_add(1);
            let value = self.hasher.hash_one(self.counter);
            if value != 0 {
                return NtpTimestamp::from_bits(value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn produces_distinct_non_zero_values() {
        let mut nonce = RandomNonce::new();
        let values: HashSet<u64> = (0..1_000).map(|_| nonce.next_nonce().to_bits()).collect();
        assert_eq!(values.len(), 1_000);
        assert!(!values.contains(&0));
    }
}
