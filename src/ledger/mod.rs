//! Stored-value balances: shop credit and gift cards.
//!
//! Both are *ledgers*, not fields. Balances are always derived from an
//! append-only list of entries, which is what makes refunds, reversals and
//! audits tractable — you can always answer "why is this balance 12.34?".

pub mod credit;
pub mod giftcard;

pub use credit::{CreditEntry, CreditEntryKind, ShopCreditAccount};
pub use giftcard::{GiftCard, GiftCardStatus, hash_gift_card_code};
