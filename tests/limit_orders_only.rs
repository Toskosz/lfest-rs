//! Test if a pure limit order strategy works correctly

use lfest::*;

#[test]
fn limit_orders_only() {
    if let Err(_) = pretty_env_logger::try_init() {}

    let config = Config {
        fee_maker: 0.0,
        fee_taker: 0.0,
        starting_balance: 1000.0,
        use_candles: false,
        leverage: 1.0,
        futures_type: FuturesType::Linear,
    };
    let mut exchange = Exchange::new(config);
    exchange.update_state(100.0, 100.1, 0);


}