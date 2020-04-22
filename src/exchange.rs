extern crate trade_aggregation;

use trade_aggregation::common::*;
use crate::orders::*;
use crate::config::*;
use chrono::prelude::*;


#[derive(Debug, Clone)]
pub struct Exchange {
    pub config: Config,
    pub position: Position,
    pub margin: Margin,
    pub acc_tracker: AccTracker,
    pub bid: f64,
    pub ask: f64,
    init: bool,
    pub total_rpnl: f64,
    pub rpnls: Vec<f64>,
    orders_done: Vec<Order>,
    pub orders_active: Vec<Order>,
    next_order_id: u64,
}

#[derive(Debug, Clone)]
pub struct Margin {
    pub wallet_balance: f64,
    pub margin_balance: f64,
    position_margin: f64,
    order_margin: f64,
    pub available_balance: f64,
}

#[derive(Debug, Clone)]
pub struct Position {
    pub size: f64,
    value: f64,
    pub entry_price: f64,
    liq_price: f64,
    margin: f64,
    pub leverage: f64,
    pub unrealized_pnl: f64,
    roe_percent: f64,
}

#[derive(Debug, Clone)]
pub struct AccTracker {
    pub num_trades: i64,
    pub num_buys: i64,
}

pub fn new(config: Config) -> Exchange {
    return Exchange{
        config,
        position: Position{
            size: 0.0,
            value: 0.0,
            entry_price: 0.0,
            liq_price: 0.0,
            margin: 0.0,
            leverage: 1.0,
            unrealized_pnl: 0.0,
            roe_percent: 0.0,
        },
        margin: Margin{
            wallet_balance: 1.0,
            margin_balance: 1.0,
            position_margin: 0.0,
            order_margin: 0.0,
            available_balance: 1.0,
        },
        acc_tracker: AccTracker{
            num_trades: 0,
            num_buys: 0,
        },
        total_rpnl: 0.0,
        bid: 0.0,
        ask: 0.0,
        init: true,
        rpnls: Vec::new(),
        orders_done: Vec::new(),
        orders_active: Vec::new(),
        next_order_id: 0,
    }
}

impl Exchange {
    // sets the new leverage of position
    // returns true if successful
    pub fn set_leverage(&mut self, l: f64) -> bool {
        if l > self.config.max_leverage {
            return false
        } else if l < self.config.min_leverage {
            return false
        }

        let new_position_margin = (self.position.value / l) + self.position.unrealized_pnl;
        if new_position_margin > self.margin.wallet_balance {
            return false
        }
        self.position.leverage = l;
        self.margin.position_margin = (self.position.value / self.position.leverage) + self.position.unrealized_pnl;
        self.margin.available_balance = self.margin.margin_balance - self.margin.order_margin - self.margin.position_margin;

        return true
    }

    // consume_candle update the exchange state with th new candle.
    // returns true if position has been liquidated
    pub fn consume_trade(&mut self, trade: &Trade) -> bool{
        if self.init {
            self.init = false;
            self.bid = trade.price;
            self.ask = trade.price;
        }
        if trade.size > 0.0 {
            self.ask = trade.price;
        } else {
            self.bid = trade.price;
        }

        if self.position.size > 0.0 {
            // liquidation check for long position
            if trade.price < self.position.liq_price {
                self.liquidate();
                return true
            }
            let upnl = self.unrealized_pnl();
            self.position.unrealized_pnl = upnl;
            self.position.roe_percent = self.roe();

        } else if self.position.size < 0.0 {
            // liquidation check for short position
            if trade.price > self.position.liq_price {
                self.liquidate();
                return true
            }
            let upnl = self.unrealized_pnl();
            self.position.unrealized_pnl = upnl;
            self.position.roe_percent = self.roe();
        }

        self.margin.margin_balance = self.margin.wallet_balance + self.position.unrealized_pnl;
        self.margin.position_margin = (self.position.value / self.position.leverage) + self.position.unrealized_pnl;
        self.margin.available_balance = self.margin.margin_balance - self.margin.position_margin;

        self.check_orders();

        return false
    }

    // consume_candle update the bid and ask price given a candle using its close price
    // returns true if position has been liquidated
    pub fn consume_candle(&mut self, candle: &Candle) -> bool {
        // TODO: consume_candle
        return false
    }

    // candle an active order
    // returns true if successful with given order_id
    pub fn cancel_order(&mut self, order_id: u64) -> Option<Order> {
        for (i, o) in self.orders_active.iter().enumerate() {
            if o.id == order_id {
                return Some(self.orders_active.remove(i));
            }
        }
        None
    }

    pub fn query_active_orders(&self, order_id: u64) -> Option<&Order> {
        for (i, o) in self.orders_active.iter().enumerate() {
            if o.id == order_id {
                return self.orders_active.get(i);
            }
        }
        None
    }

    pub fn buy_market(&mut self, amount_base: i64) -> bool {
        assert!(amount_base > 0);
        // TODO: return false if not valid amount_base instead of panic

        let fee_base = (self.config.fee_taker * amount_base as f64).round();
        let fee_asset = fee_base / self.ask;

        let add_margin = amount_base as f64 / self.ask / self.position.leverage;
        if add_margin + fee_asset > self.margin.available_balance {
            return false
        }

        if self.position.size < 0.0 {
            let rpnl = (amount_base as f64 - fee_base) * (1.0 / self.position.entry_price - 1.0 / self.ask);
            self.total_rpnl += rpnl;
            self.margin.wallet_balance += rpnl;

            self.margin.available_balance += add_margin - fee_asset;
            self.position.margin -= add_margin;

            self.rpnls.push(rpnl);

        } else {
            self.margin.available_balance -= add_margin - fee_asset;
            self.position.margin += add_margin;
        }

        self.position.entry_price = ((self.ask * amount_base as f64)
            + (self.position.entry_price * self.position.size.abs() ))
            / (amount_base as f64 + self.position.size.abs());

        self.position.size += amount_base as f64;
        self.position.value = self.position.size.abs() / self.ask;

        let upnl = self.unrealized_pnl();
        self.position.unrealized_pnl = upnl;
        self.position.roe_percent = self.roe();

        self.update_liq_price();

        self.margin.position_margin = (self.position.value / self.position.leverage) + self.position.unrealized_pnl;
        self.margin.margin_balance = self.margin.wallet_balance + self.position.unrealized_pnl;

        self.acc_tracker.num_buys += 1;
        self.acc_tracker.num_trades += 1;

        return true
    }

    pub fn sell_market(&mut self, amount_base: i64) -> bool {
        assert!(amount_base > 0);
        // TODO: return false if not valid amount_base instead of panic

        let fee_base = self.config.fee_taker * amount_base as f64;
        let fee_asset = fee_base / self.bid;

        let add_margin = amount_base as f64 / self.bid / self.position.leverage;
        if add_margin + fee_asset > self.margin.available_balance {
            return false
        }

        if self.position.size > 0.0 {
            let rpnl = (amount_base as f64 - fee_base) * (1.0 / self.bid - 1.0 / self.position.entry_price) ;
            self.total_rpnl += rpnl;
            self.margin.wallet_balance += rpnl;
            self.margin.available_balance += add_margin - fee_asset;
            self.position.margin -= add_margin;

            self.rpnls.push(rpnl);

        } else {
            self.margin.available_balance -= add_margin - fee_asset;
            self.position.margin += add_margin;
        }

        self.position.entry_price = ((self.bid * amount_base as f64) + self.position.entry_price * self.position.size.abs() as f64)
            / (amount_base as f64 + self.position.size.abs() as f64);

        self.position.size -= amount_base as f64;
        self.position.value = self.position.size.abs() as f64 / self.bid;

        let upnl = self.unrealized_pnl();
        self.position.unrealized_pnl = upnl;
        self.position.roe_percent = self.roe();

        self.update_liq_price();

        self.margin.position_margin = (self.position.value / self.position.leverage) + self.position.unrealized_pnl;
        self.margin.margin_balance = self.margin.wallet_balance + self.position.unrealized_pnl;

        self.acc_tracker.num_trades += 1;

        return true
    }

    pub fn submit_order(&mut self, mut o: Order) -> Option<OrderError> {
        if self.orders_active.len() >= self.config.max_active_orders {
            return Some(OrderError::MaxActiveOrders)
        }
        let valid: bool = match o.order_type {
            OrderType::Market => true,
            OrderType::Limit => panic!("not implemented yet"),
            OrderType::StopMarket => self.validate_stop_market_order(&o),
            OrderType::StopLimit => panic!("stop limit orders not implemented yet"),
            OrderType::TakeProfitLimit => panic!("take profit limit orders not implemented yet"),
            OrderType::TakeProfitMarket => self.validate_take_profit_market_order(&o),
        };
        if !valid {
            return Some(OrderError::InvalidOrder)
        }
        // check if enough available balance for initial margin requirements
        let init_margin = o.size / self.bid / self.position.leverage;
        if init_margin > self.margin.available_balance {
            return Some(OrderError::NotEnoughAvailableBalance)
        }
        // increase order_margin
        self.margin.order_margin += order_margin;
        self.margin.available_balance -= order_margin;

        // assign unique order id
        o.id = self.next_order_id;
        self.next_order_id += 1;

        // assign timestamp
        let now = Utc::now();
        o.timestamp = now.timestamp_millis() as u64;
        self.orders_active.push(o);

        return None
    }

    pub fn unrealized_pnl(&self) -> f64 {
        if self.position.size == 0.0 {
            return 0.0;
        } else if self.position.size > 0.0 {
            return (1.0 / self.position.entry_price - 1.0 / self.bid) * self.position.size.abs() as f64;
        } else {
            return (1.0 / self.ask - 1.0 / self.position.entry_price) * self.position.size.abs() as f64;
        }
    }

    pub fn num_active_orders(&self) -> usize {
        return self.orders_active.len()
    }

    fn liquidate(&mut self) {
        if self.position.size > 0.0 {
            let liq_margin = self.position.size.abs() / self.bid / self.position.leverage;

            let rpnl = self.position.size.abs() * (1.0 / self.position.entry_price - 1.0 / self.bid);
            self.total_rpnl += rpnl;
            self.margin.wallet_balance += rpnl;

            self.margin.available_balance -= liq_margin;
            self.position.margin = 0.0;

            self.position.entry_price = 0.0;

            self.position.size = 0.0;
            self.position.value = 0.0;

            self.position.unrealized_pnl = 0.0;
            self.position.roe_percent = 0.0;

            self.update_liq_price();

            self.margin.position_margin = (self.position.value / self.position.leverage) + self.position.unrealized_pnl;
            self.margin.margin_balance = self.margin.wallet_balance + self.position.unrealized_pnl;

            self.acc_tracker.num_trades += 1;

        } else {
            let liq_margin = self.position.size.abs() / self.ask / self.position.leverage;

            let rpnl = self.position.size.abs() * (1.0 / self.position.entry_price - 1.0 / self.ask);
            self.total_rpnl += rpnl;
            self.margin.wallet_balance += rpnl;

            self.margin.available_balance -= liq_margin;
            self.position.margin = 0.0;

            self.position.entry_price = 0.0;

            self.position.size = 0.0;
            self.position.value = 0.0;

            self.position.unrealized_pnl = 0.0;
            self.position.roe_percent = 0.0;

            self.update_liq_price();

            self.margin.position_margin = (self.position.value / self.position.leverage) + self.position.unrealized_pnl;
            self.margin.margin_balance = self.margin.wallet_balance + self.position.unrealized_pnl;

            self.acc_tracker.num_buys += 1;
            self.acc_tracker.num_trades += 1;
        }

        self.margin.margin_balance = self.margin.wallet_balance + self.position.unrealized_pnl;
        self.margin.position_margin = (self.position.value / self.position.leverage) + self.position.unrealized_pnl;
        self.margin.available_balance = self.margin.margin_balance - self.margin.position_margin;

    }

    fn check_orders(&mut self) {
        for i in 0..self.orders_active.len() {
            match self.orders_active[i].order_type {
                OrderType::Market => {},
                OrderType::Limit => panic!("limit order not implemented yet"),
                OrderType::StopMarket => self.handle_stop_market_order(i),
                OrderType::StopLimit => panic!("stop limit orders not implemented yet"),
                OrderType::TakeProfitLimit => panic!("take profit limit orders not implemented yet"),
                OrderType::TakeProfitMarket => self.handle_take_profit_market_order(i),
            }
        }
        // move executed orders from orders_active to orders_done
        let mut i: usize = 0;
        loop {
            if i >= self.orders_active.len() {
                break
            }
            if self.orders_active[i].done() {
                self.orders_active.remove(i);
                // self.orders_done.push(done_order);
            }
            i += 1;
        }
    }

    fn handle_stop_market_order(&mut self, order_index: usize) {
        let o: &Order = &self.orders_active[order_index];
        match o.side {
            Side::Buy => { if o.price > self.ask { return }
                self.buy_market((o.size * self.ask) as i64);
                self.orders_active[order_index].mark_done();
            },
            Side::Sell => { if o.price > self.bid { return }
                self.sell_market((o.size * self.bid) as i64);
                self.orders_active[order_index].mark_done();
            },
        }
    }

    fn handle_take_profit_market_order(&mut self, order_index: usize) {
        let o: &Order = &self.orders_active[order_index];
        match o.side {
            Side::Buy => { if o.price < self.bid { return }
                self.buy_market((o.size * self.ask) as i64);
                self.orders_active[order_index].mark_done();
            },
            Side::Sell => { if o.price > self.ask { return }
                self.sell_market((o.size * self.bid) as i64);
                self.orders_active[order_index].mark_done();
            },
        }
    }

    // returns true if order is valid
    fn validate_stop_market_order(&self, o: &Order) -> bool {
        return match o.side {
            Side::Buy => { if o.price <= self.ask { return false }
                true
            },
            Side::Sell => { if o.price >= self.bid { return false }
                true
            },
        }
    }

    fn validate_take_profit_market_order(&self, o: &Order) -> bool {
        return match o.side {
            Side::Buy => { if o.price > self.bid { return false}
                true
            },
            Side::Sell => { if o.price < self.ask { return false }
                true
            },
        }
    }

    fn update_liq_price(&mut self) {
        if self.position.size == 0.0 {
            self.position.liq_price = 0.0;
        } else if self.position.size > 0.0 {
            self.position.liq_price = self.position.entry_price - (self.position.entry_price / self.position.leverage);
        } else {
            self.position.liq_price = self.position.entry_price + (self.position.entry_price / self.position.leverage);
        }
    }

    fn roe(&self) -> f64 {
        if self.position.size > 0.0 {
            return (self.bid - self.position.entry_price) / self.position.entry_price;
        } else {
            return (self.position.entry_price - self.ask) / self.position.entry_price;
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buy_market() {

    }

    #[test]
    fn sell_market() {

    }

    #[test]
    fn liq_price() {

    }

    #[test]
    fn unrealize_pnl() {

    }

    #[test]
    fn roe() {

    }

    #[test]
    fn test_validate_stop_market_order() {
        let config = Config::xbt_usd();
        let mut exchange = new(config);
        let t = Trade{
            timestamp: 0,
            price: 1000.0,
            size: 100.0,
        };
        exchange.consume_trade(&t);

        let o = Order::stop_market(Side::Buy, 1010.0, 10.0);
        let valid = exchange.validate_stop_market_order(&o);
        assert!(valid);

        let o = Order::stop_market(Side::Sell, 1010.0, 10.0);
        let valid = exchange.validate_stop_market_order(&o);
        assert!(!valid);

        let o = Order::stop_market(Side::Buy, 980.0, 10.0);
        let valid = exchange.validate_stop_market_order(&o);
        assert!(!valid);

        let o = Order::market(Side::Sell, 980.0, 10.0);
        let valid = exchange.validate_stop_market_order(&o);
        assert!(valid);

        let o = Order::stop_market(Side::Buy, 1000.0, 10.0);
        let valid = exchange.validate_stop_market_order(&o);
        assert!(!valid);

        let o = Order::stop_market(Side::Buy, 1000.0, 10.0);
        let valid = exchange.validate_stop_market_order(&o);
        assert!(!valid);
    }

    #[test]
    fn test_validate_take_profit_market_order() {
        let config = Config::xbt_usd();
        let mut exchange = new(config);
        let t = Trade{
            timestamp: 0,
            price: 1000.0,
            size: 100.0,
        };
        exchange.consume_trade(&t);

        let o = Order::take_profit_market(Side::Buy, 950.0, 10.0);
        let valid = exchange.validate_take_profit_market_order(&o);
        assert!(valid);

        let o = Order::take_profit_market(Side::Sell, 950.0, 10.0);
        let valid = exchange.validate_take_profit_market_order(&o);
        assert!(!valid);

        let o = Order::take_profit_market(Side::Buy, 1050.0, 10.0);
        let valid = exchange.validate_take_profit_market_order(&o);
        assert!(!valid);

        let o = Order::take_profit_market(Side::Sell, 1050.0, 10.0);
        let valid = exchange.validate_take_profit_market_order(&o);
        assert!(valid);
    }

    #[test]
    fn test_handle_stop_market_order() {
        let config = Config::xbt_usd();
        let mut exchange = new(config);
        let t = Trade{
            timestamp: 0,
            price: 1000.0,
            size: 100.0,
        };
        exchange.consume_trade(&t);

        let o = Order::stop_market(Side::Buy, 1050.0, 10.0);
        let valid = exchange.submit_order(o);
        match valid {
            Some(_) => {},
            None => panic!("order not valid!")
        }
        exchange.handle_stop_market_order(0);
    }

    #[test]
    fn test_order_ids() {
        let config = Config::xbt_usd();
        let mut exchange = new(config);
        let t = Trade{
            timestamp: 0,
            price: 100.0,
            size: 10.0,
        };
        exchange.consume_trade(&t);
        for i in 0..100 {
            let o = Order::stop_market(Side::Buy, 101.0 + i as f64, 10.0);
            exchange.submit_order(o);
        }
        let active_orders = exchange.orders_active;
        let mut last_order_id: i64 = -1;
        for o in &active_orders {
            assert!(o.id as i64 > last_order_id);
            last_order_id = o.id as i64;
        }
    }

    #[test]
    fn test_set_leverage() {
        // TODO:
    }
}
