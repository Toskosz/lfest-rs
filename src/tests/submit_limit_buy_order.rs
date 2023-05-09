use crate::{mock_exchange_base, prelude::*};

#[test]
fn submit_limit_buy_order_no_position() {
    let mut exchange = mock_exchange_base();
    assert_eq!(
        exchange
            .update_state(0, bba!(quote!(99), quote!(100)))
            .unwrap(),
        vec![]
    );

    let mut order = Order::limit(Side::Buy, quote!(98), base!(5)).unwrap();
    exchange.submit_order(order.clone()).unwrap();

    assert_eq!(
        exchange.account().position,
        Position {
            size: base!(0),
            entry_price: quote!(0),
            position_margin: quote!(0),
            leverage: leverage!(1),
        }
    );

    // Now fill the order
    order.set_id(0);
    assert_eq!(
        exchange
            .update_state(0, bba!(quote!(96), quote!(97)))
            .unwrap(),
        vec![order]
    );
    assert_eq!(
        exchange.account().position,
        Position {
            size: base!(5),
            entry_price: quote!(98),
            position_margin: quote!(490),
            leverage: leverage!(1),
        }
    );
    let fee = quote!(0.098);
    assert_eq!(exchange.account().wallet_balance, quote!(1000) - fee);
    assert_eq!(exchange.account().available_balance(), quote!(510) - fee);

    // close the position again
    let mut order = Order::limit(Side::Sell, quote!(98), base!(5)).unwrap();
    exchange.submit_order(order.clone()).unwrap();

    assert_eq!(
        exchange
            .update_state(0, bba!(quote!(96), quote!(97)))
            .unwrap(),
        vec![]
    );

    order.set_id(1);
    assert_eq!(
        exchange
            .update_state(0, bba!(quote!(99), quote!(100)))
            .unwrap(),
        vec![order]
    );
    assert_eq!(
        exchange.account().position,
        Position {
            size: base!(0),
            entry_price: quote!(98),
            position_margin: quote!(0),
            leverage: leverage!(1),
        }
    );
    assert_eq!(exchange.account().wallet_balance, quote!(1000) - fee - fee);
    assert_eq!(
        exchange.account().available_balance(),
        quote!(1000) - fee - fee
    );
}

// Test there is a maximum quantity of buy orders the account can post.
#[test]
fn submit_limit_buy_order_no_position_max() {
    let mut exchange = mock_exchange_base();
    assert_eq!(
        exchange
            .update_state(0, bba!(quote!(99), quote!(100)))
            .unwrap(),
        vec![]
    );

    let mut order = Order::limit(Side::Buy, quote!(100), base!(5)).unwrap();
    exchange.submit_order(order.clone()).unwrap();
    let mut order = Order::limit(Side::Buy, quote!(100), base!(4)).unwrap();
    exchange.submit_order(order.clone()).unwrap();
    let mut order = Order::limit(Side::Buy, quote!(100), base!(1)).unwrap();
    assert_eq!(
        exchange.submit_order(order.clone()),
        Err(Error::RiskError(RiskError::NotEnoughAvailableBalance))
    );
}
