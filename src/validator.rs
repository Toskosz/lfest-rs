use crate::{
    account::Account,
    account_tracker::AccountTracker,
    errors::OrderError,
    limit_order_margin::order_margin,
    quote,
    types::{Currency, Fee, MarginCurrency, Order, QuoteCurrency, Side},
    utils::{max, min},
    Decimal,
};

#[derive(Clone, Debug, Default)]
/// Used for validating orders
pub(crate) struct Validator {
    fee_maker: Fee,
    fee_taker: Fee,
    bid: QuoteCurrency,
    ask: QuoteCurrency,
    max_num_open_orders: usize,
}

impl Validator {
    /// Create a new Validator with a given fee maker and taker
    #[inline]
    pub(crate) fn new(fee_maker: Fee, fee_taker: Fee, max_num_open_orders: usize) -> Self {
        Self {
            fee_maker,
            fee_taker,
            bid: quote!(0.0),
            ask: quote!(0.0),
            max_num_open_orders,
        }
    }

    /// update the state with newest information
    #[inline]
    pub(crate) fn update(&mut self, bid: QuoteCurrency, ask: QuoteCurrency) {
        debug_assert!(bid <= ask, "Make sure bid <= ask");

        self.bid = bid;
        self.ask = ask;
    }

    /// Check if market order is correct.
    ///
    /// # Returns:
    /// debited and credited account balance deltas, if order valid,
    /// [`OrderError`] otherwise
    pub(crate) fn validate_market_order<A, S>(
        &self,
        o: &Order<S>,
        acc: &Account<A, S>,
    ) -> Result<(), OrderError>
    where
        A: AccountTracker<S::PairedCurrency>,
        S: Currency,
        S::PairedCurrency: MarginCurrency,
    {
        let (debit, credit) = self.order_cost_market(o, acc);
        debug!("validate_market_order debit: {}, credit: {}", debit, credit);

        if credit > acc.margin().available_balance() + debit {
            return Err(OrderError::NotEnoughAvailableBalance);
        }

        Ok(())
    }

    /// Check if a limit order is correct.
    ///
    /// # Returns:
    /// order margin if order is valid, [`OrderError`] otherwise
    pub(crate) fn validate_limit_order<A, S>(
        &self,
        o: &Order<S>,
        acc: &Account<A, S>,
    ) -> Result<S::PairedCurrency, OrderError>
    where
        A: AccountTracker<S::PairedCurrency>,
        S: Currency,
        S::PairedCurrency: MarginCurrency,
    {
        if acc.num_active_limit_orders() >= self.max_num_open_orders {
            return Err(OrderError::MaxActiveOrders);
        }
        // validate order price
        let limit_price = o.limit_price().expect("The limit order must contain a price; qed");
        match o.side() {
            Side::Buy => {
                if limit_price > self.ask {
                    return Err(OrderError::LimitPriceLargerThanAsk);
                }
            }
            Side::Sell => {
                if limit_price < self.bid {
                    return Err(OrderError::LimitPriceLowerThanBid);
                }
            }
        }

        let order_margin = self.limit_order_margin_cost(o, acc);

        debug!(
            "validate_limit_order order_margin: {}, ab: {}",
            order_margin,
            acc.margin().available_balance(),
        );
        if order_margin > acc.margin().available_balance() {
            return Err(OrderError::NotEnoughAvailableBalance);
        }

        Ok(order_margin)
    }

    /// Compute the order cost of a market order
    ///
    /// # Returns:
    /// debited and credited account balance delta
    #[must_use]
    fn order_cost_market<A, S>(
        &self,
        order: &Order<S>,
        acc: &Account<A, S>,
    ) -> (S::PairedCurrency, S::PairedCurrency)
    where
        A: AccountTracker<S::PairedCurrency>,
        S: Currency,
        S::PairedCurrency: MarginCurrency,
    {
        debug!("order_cost_market: order: {:?},\nacc.position: {:?}", order, acc.position());

        let pos_size = acc.position().size();

        let (mut debit, mut credit) = if pos_size.is_zero() {
            match order.side() {
                Side::Buy => (min(order.quantity(), acc.open_limit_sell_size()), order.quantity()),
                Side::Sell => (min(order.quantity(), acc.open_limit_buy_size()), order.quantity()),
            }
        } else if pos_size > S::new_zero() {
            match order.side() {
                Side::Buy => (S::new_zero(), order.quantity()),
                Side::Sell => (
                    min(order.quantity(), acc.position().size()),
                    max(S::new_zero(), order.quantity() - acc.position().size()),
                ),
            }
        } else {
            match order.side() {
                Side::Buy => (
                    min(order.quantity(), acc.position().size().abs()),
                    max(S::new_zero(), order.quantity() - acc.position().size().abs()),
                ),
                Side::Sell => (S::new_zero(), order.quantity()),
            }
        };
        let l = acc.position().leverage().inner();
        debit = S::new(debit.inner() / l);
        credit = S::new(credit.inner() / l);

        let fee_of_size: S = order.quantity().fee_portion(self.fee_taker);

        let price = match order.side() {
            Side::Buy => self.ask,
            Side::Sell => self.bid,
        };
        (debit.convert(price), credit.convert(price) + fee_of_size.convert(price))
    }

    /// Compute the order cost of a limit order
    ///
    /// # Returns;
    /// debited and credited account balance delta, measured in the margin
    /// currency
    #[must_use]
    fn limit_order_margin_cost<A, S>(
        &self,
        order: &Order<S>,
        acc: &Account<A, S>,
    ) -> S::PairedCurrency
    where
        A: AccountTracker<S::PairedCurrency>,
        S: Currency,
        S::PairedCurrency: MarginCurrency,
    {
        let mut orders = acc.active_limit_orders().clone();
        debug!("limit_order_margin_cost: order: {:?}, active_limit_orders: {:?}", order, orders);
        orders.insert(order.id(), order.clone());
        let needed_order_margin =
            order_margin(orders.values().cloned(), acc.position(), self.fee_maker);

        // get the additional needed difference
        let diff = needed_order_margin - acc.margin().order_margin();
        debug!(
            "needed_order_margin: {}, acc_om: {}, diff: {}",
            needed_order_margin,
            acc.margin().order_margin(),
            diff
        );

        diff
    }
}

#[cfg(test)]
mod tests {
    use fpdec::Decimal;

    use super::*;
    use crate::{account_tracker::NoAccountTracker, prelude::*};

    #[test]
    fn validate_inverse_futures_market_order_without_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0002), fee!(0.0006), 100);
        validator.update(quote!(100.0), quote!(101.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            let acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));

            let o = Order::market(Side::Buy, QuoteCurrency::new(Decimal::from(40) * l)).unwrap();
            validator.validate_market_order(&o, &acc).unwrap();

            let o = Order::market(Side::Buy, QuoteCurrency::new(Decimal::from(105) * l)).unwrap();
            assert!(validator.validate_market_order(&o, &acc).is_err());

            let o = Order::market(Side::Sell, QuoteCurrency::new(Decimal::from(40) * l)).unwrap();
            validator.validate_market_order(&o, &acc).unwrap();

            let o = Order::market(Side::Sell, QuoteCurrency::new(Decimal::from(105) * l)).unwrap();
            assert!(validator.validate_market_order(&o, &acc).is_err());
        }
    }

    #[test]
    fn validate_inverse_futures_market_order_with_long_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0002), fee!(0.0006), 100);
        validator.update(quote!(100.0), quote!(101.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("testing with leverage: {}", l);

            // with long position
            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Buy,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(101.0),
            );

            let o = Order::market(Side::Buy, QuoteCurrency::new(Decimal::from(49) * l)).unwrap();
            validator.validate_market_order(&o, &acc).unwrap();

            let o = Order::market(Side::Buy, QuoteCurrency::new(Decimal::from(51) * l)).unwrap();
            assert!(validator.validate_market_order(&o, &acc).is_err());

            let o = Order::market(Side::Sell, QuoteCurrency::new(Decimal::from(149) * l)).unwrap();
            validator.validate_market_order(&o, &acc).unwrap();

            let o = Order::market(Side::Sell, QuoteCurrency::new(Decimal::from(151) * l)).unwrap();
            assert!(validator.validate_market_order(&o, &acc).is_err());
        }
    }

    #[test]
    fn validate_inverse_futures_market_order_with_short_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0002), fee!(0.0006), 100);
        validator.update(quote!(100.0), quote!(101.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            // with short position
            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Sell,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(100.0),
            );

            let o = Order::market(Side::Buy, QuoteCurrency::new(Decimal::from(149) * l)).unwrap();
            validator.validate_market_order(&o, &acc).unwrap();

            let o = Order::market(Side::Buy, QuoteCurrency::new(Decimal::from(151) * l)).unwrap();
            assert!(validator.validate_market_order(&o, &acc).is_err());

            let o = Order::market(Side::Sell, QuoteCurrency::new(Decimal::from(49) * l)).unwrap();
            validator.validate_market_order(&o, &acc).unwrap();

            let o = Order::market(Side::Sell, QuoteCurrency::new(Decimal::from(51) * l)).unwrap();
            assert!(validator.validate_market_order(&o, &acc).is_err());
        }
    }

    #[test]
    fn validate_inverse_futures_market_order_with_open_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0002), fee!(0.0006), 100);
        validator.update(quote!(100.0), quote!(101.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            // with buy limit order
            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            let mut o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o = Order::market(Side::Buy, QuoteCurrency::new(Decimal::from(49) * l)).unwrap();
            validator.validate_market_order(&o, &acc).unwrap();

            let o = Order::market(Side::Buy, QuoteCurrency::new(Decimal::from(51) * l)).unwrap();
            assert!(validator.validate_market_order(&o, &acc).is_err());

            let o = Order::market(Side::Sell, QuoteCurrency::new(Decimal::from(99) * l)).unwrap();
            validator.validate_market_order(&o, &acc).unwrap();

            let o = Order::market(Side::Sell, QuoteCurrency::new(Decimal::from(101) * l)).unwrap();
            assert!(validator.validate_market_order(&o, &acc).is_err());

            // with sell limit order
            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(2); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o = Order::market(Side::Buy, QuoteCurrency::new(Decimal::from(99) * l)).unwrap();
            validator.validate_market_order(&o, &acc).unwrap();

            let o = Order::market(Side::Buy, QuoteCurrency::new(Decimal::from(101) * l)).unwrap();
            assert!(validator.validate_market_order(&o, &acc).is_err());

            let o = Order::market(Side::Sell, QuoteCurrency::new(Decimal::from(49) * l)).unwrap();
            validator.validate_market_order(&o, &acc).unwrap();

            let o = Order::market(Side::Sell, QuoteCurrency::new(Decimal::from(51) * l)).unwrap();
            assert!(validator.validate_market_order(&o, &acc).is_err());
        }
    }

    #[test]
    fn validate_inverse_futures_limit_order_without_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0002), fee!(0.0006), 100);
        validator.update(quote!(100.0), quote!(101.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            let acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(99) * l))
                    .unwrap();
            validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(101) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());

            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(99) * l))
                    .unwrap();
            validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(101) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());
        }
    }

    #[test]
    fn validate_inverse_futures_limit_order_with_long_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0002), fee!(0.0006), 100);
        validator.update(quote!(100.0), quote!(101.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            // with long position
            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Buy,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(101.0),
            );

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(49) * l))
                    .unwrap();
            validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(51) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());

            // TODO: should this work with 149?
            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(99) * l))
                    .unwrap();
            validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(101) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());
        }
    }

    #[test]
    fn validate_inverse_futures_limit_order_with_short_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0002), fee!(0.0006), 100);
        validator.update(quote!(100.0), quote!(101.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            // with short position
            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Sell,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(100.0),
            );

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(99) * l))
                    .unwrap();
            validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(101) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());

            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(49) * l))
                    .unwrap();
            validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(51) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());
        }
    }

    #[test]
    fn validate_inverse_futures_limit_order_with_open_buy_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0002), fee!(0.0006), 100);
        validator.update(quote!(100.0), quote!(101.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            // with open buy order
            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            let mut o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(49) * l))
                    .unwrap();
            let _ = validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(51) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());

            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(99) * l))
                    .unwrap();
            let _ = validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(101) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());
        }
    }

    #[test]
    fn validate_inverse_futures_limit_order_with_open_sell_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0002), fee!(0.0006), 100);
        validator.update(quote!(100.0), quote!(101.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            // with open sell order
            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            let mut o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(99) * l))
                    .unwrap();
            validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(101) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());

            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(49) * l))
                    .unwrap();
            validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(51) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());
        }
    }

    #[test]
    fn validate_inverse_futures_limit_order_with_open_orders_mixed() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0002), fee!(0.0006), 100);
        validator.update(quote!(100.0), quote!(101.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            // with mixed limit orders
            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            let mut o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);
            let mut o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(2); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);
            // not sure if the fees of both orders should be included in order_margin
            //let fee = (0.0002 * 50.0 * l / 100.0) + (0.0002 * 50.0 * l / 101.0);
            //assert_eq!(round(acc.margin().order_margin(), 4), round(0.5 + fee, 4));

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(49) * l))
                    .unwrap();
            validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(51) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());

            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(49) * l))
                    .unwrap();
            validator.validate_limit_order(&o, &acc).unwrap();

            let o =
                Order::limit(Side::Sell, quote!(101.0), QuoteCurrency::new(Decimal::from(51) * l))
                    .unwrap();
            assert!(validator.validate_limit_order(&o, &acc).is_err());
        }
    }

    #[test]
    #[should_panic]
    // basically you should not be able to endlessly add both buy and sell orders
    fn validate_inverse_futures_limit_order_with_open_orders_mixed_panic() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0002), fee!(0.0006), 100);
        validator.update(quote!(100.0), quote!(101.0));
        let mut acc = Account::new(NoAccountTracker::default(), leverage!(1.0), base!(1.0));

        for i in (0..100).step_by(2) {
            // with mixed limit orders
            let mut o = Order::limit(Side::Buy, quote!(100.0), quote!(50.0)).unwrap();
            o.set_id(i); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);
            let mut o = Order::limit(Side::Sell, quote!(101.0), quote!(50.0)).unwrap();
            o.set_id(i + 1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);
        }
        debug!(
            "final account olbs: {}, olss: {:?}",
            acc.open_limit_buy_size(),
            acc.open_limit_sell_size()
        );
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_without_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let acc = Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));
        }
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_with_long_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc =
                Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));
            acc.change_position(Side::Buy, BaseCurrency::new(l / Decimal::TWO), quote!(100.0));

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(0.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
        }
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_with_short_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc =
                Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));
            acc.change_position(Side::Sell, BaseCurrency::new(l / Decimal::TWO), quote!(100.0));

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(0.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));
        }
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_with_open_buy_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc =
                Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));
            let mut o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(0.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
        }
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_with_open_sell_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc =
                Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(0.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));
        }
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_with_mixed_open_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc =
                Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));
            let mut o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                    .unwrap();
            o.set_id(2); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));
        }
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_with_long_position_and_open_buy_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc =
                Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));
            acc.change_position(Side::Buy, BaseCurrency::new(l / Decimal::TWO), quote!(100.0));
            let mut o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(0.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(0.0));
        }
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_with_long_position_and_open_sell_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc =
                Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));
            acc.change_position(Side::Buy, BaseCurrency::new(l / Decimal::TWO), quote!(100.0));
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));
        }
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_with_long_position_and_mixed_open_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc =
                Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));
            acc.change_position(Side::Buy, BaseCurrency::new(l / Decimal::TWO), quote!(100.0));
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);
            let mut o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            o.set_id(2); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(0.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
        }
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_with_short_position_and_open_buy_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc =
                Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));
            acc.change_position(Side::Sell, BaseCurrency::new(l / Decimal::TWO), quote!(100.0));
            let mut o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));
        }
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_with_short_position_and_open_sell_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc =
                Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));
            acc.change_position(Side::Sell, BaseCurrency::new(l / Decimal::TWO), quote!(100.0));
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(0.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(0.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));
        }
    }

    #[test]
    fn linear_futures_limit_order_margin_cost_with_short_position_and_mixed_open_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc =
                Account::new(NoAccountTracker::default(), Leverage::new(l), quote!(100.0));
            acc.change_position(Side::Sell, BaseCurrency::new(l / Decimal::TWO), quote!(100.0));
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);
            let mut o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            o.set_id(2); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(0.0));
            let o = Order::limit(Side::Buy, quote!(100.0), BaseCurrency::new(l)).unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));

            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(l / Decimal::TWO))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(50.0));
            let o = Order::limit(Side::Sell, quote!(100.0), BaseCurrency::new(Decimal::ONE * l))
                .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), quote!(100.0));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_without_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_with_long_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Buy,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(100.0),
            );

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.0));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_with_short_position() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Sell,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(100.0),
            );

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.0));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_with_open_buy_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            let mut o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.0));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_with_open_sell_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.0));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_with_mixed_open_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            let mut o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(2); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_with_long_position_and_open_buy_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Buy,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(100.0),
            );
            let mut o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.0));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.0));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_with_long_position_and_open_sell_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Buy,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(100.0),
            );
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_with_long_position_and_mixed_open_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Buy,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(100.0),
            );
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);
            let mut o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(2); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.0));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_with_short_position_and_open_buy_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Sell,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(100.0),
            );
            let mut o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_with_short_position_and_open_sell_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Sell,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(100.0),
            );
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.0));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.0));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));
        }
    }

    #[test]
    fn inverse_futures_limit_order_margin_cost_with_short_position_and_mixed_open_orders() {
        if let Err(_) = pretty_env_logger::try_init() {}

        let mut validator = Validator::new(fee!(0.0), fee!(0.0), 100);
        validator.update(quote!(100.0), quote!(100.0));

        for l in (1..5).map(|v| Decimal::from(v)) {
            debug!("leverage: {}", l);

            let mut acc = Account::new(NoAccountTracker::default(), Leverage::new(l), base!(1.0));
            acc.change_position(
                Side::Sell,
                QuoteCurrency::new(Decimal::from(50) * l),
                quote!(100.0),
            );
            let mut o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(1); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);
            let mut o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            o.set_id(2); // different id from test orders
            let order_margin = validator.validate_limit_order(&o, &acc).unwrap();
            acc.append_limit_order(o, order_margin);

            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.0));
            let o =
                Order::limit(Side::Buy, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));

            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(50) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(0.5));
            let o =
                Order::limit(Side::Sell, quote!(100.0), QuoteCurrency::new(Decimal::from(100) * l))
                    .unwrap();
            assert_eq!(validator.limit_order_margin_cost(&o, &acc), base!(1.0));
        }
    }
}
