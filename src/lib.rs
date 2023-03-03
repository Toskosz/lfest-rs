#![deny(missing_docs, rustdoc::missing_crate_level_docs, unused_imports)]
#![warn(clippy::all)]

//! lfest - leveraged futures exchange for simulated trading
//! aims to be a high performance exchange for backtesting

#[macro_use]
extern crate log;
#[macro_use]
extern crate serde;

mod account;
pub mod account_tracker;
mod config;
mod cornish_fisher;
mod errors;
mod exchange;
mod limit_order_margin;
mod margin;
// TODO: finish the feature
// mod order_filters;
mod position;
mod types;
mod utils;
mod validator;

use fpdec::Decimal;

/// Exports common types
pub mod prelude {
    // Too make the macros work
    pub use fpdec::Decimal;

    pub use crate::{
        account::Account,
        account_tracker::AccountTracker,
        base,
        bba,
        config::Config,
        errors::{Error, OrderError, Result},
        exchange::Exchange,
        fee,
        leverage,
        margin::Margin,
        // TODO: finish the feature
        // order_filters::{PriceFilter, QuantityFilter},
        position::Position,
        quote,
        types::*,
    };
}
