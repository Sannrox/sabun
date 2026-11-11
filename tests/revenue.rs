//! Revenue per customer city for orders above a threshold.
//!
//! Filter orders, join customers, map to `(city, amount)`, sum per city.
//! Join stays binary; the unary tail is a static [`Operator`] pipeline.
//! A view that starts after a table already has rows replays those rows as
//! one change, then takes incremental steps.

use std::collections::BTreeMap;

use sabun::aggregate::Sum;
use sabun::circuit::{Map, Operator};
use sabun::join::Join;
use sabun::zset::ZSet;

const THRESHOLD: i64 = 500;

type CustomerId = u32;
type OrderId = u32;
type Cents = i64;
type City = &'static str;
type Order = (CustomerId, (OrderId, Cents));
type Customer = (CustomerId, City);

type Joined = (CustomerId, (OrderId, Cents), City);
type CityAmount = fn(&Joined) -> (City, Cents);
type Tail = sabun::circuit::Then<Map<Joined, (City, Cents), CityAmount>, Sum<City, Cents>>;

struct Revenue<Rest> {
    join: Join<CustomerId, (OrderId, Cents), City>,
    rest: Rest,
}

impl Revenue<Tail> {
    fn new() -> Self {
        Self {
            join: Join::new(),
            rest: Map::new(to_city_amount as CityAmount).then(Sum::new()),
        }
    }
}

impl<Rest> Revenue<Rest>
where
    Rest: Operator<In = (CustomerId, (OrderId, Cents), City), Out = (City, Cents)>,
    Rest::Error: std::fmt::Debug,
{
    fn step(&mut self, d_orders: ZSet<Order>, d_customers: ZSet<Customer>) -> ZSet<(City, Cents)> {
        let filtered = d_orders.filter(|(_, (_, cents))| *cents >= THRESHOLD);
        let joined = self.join.step(filtered, d_customers);
        self.rest
            .step(joined)
            .expect("tiny example does not overflow")
    }
}

fn to_city_amount(row: &(CustomerId, (OrderId, Cents), City)) -> (City, Cents) {
    let (_, (_, cents), city) = row;
    (*city, *cents)
}

fn rows<T: Ord>(rows: impl IntoIterator<Item = (T, i64)>) -> ZSet<T> {
    rows.into_iter().collect()
}

/// Nested-loop revenue on the finished tables. Shares no code with [`Revenue`].
fn revenue_reference(orders: &ZSet<Order>, customers: &ZSet<Customer>) -> ZSet<(City, Cents)> {
    let mut totals: BTreeMap<City, (i64, i64)> = BTreeMap::new();
    for ((customer, (_, cents)), order_weight) in orders.iter() {
        if *cents < THRESHOLD {
            continue;
        }
        for ((id, city), city_weight) in customers.iter() {
            if id != customer {
                continue;
            }
            let weight = order_weight * city_weight;
            let slot = totals.entry(*city).or_insert((0, 0));
            slot.0 += weight;
            slot.1 += cents * weight;
        }
    }
    totals
        .into_iter()
        .filter(|(_, (count, _))| *count != 0)
        .map(|(city, (_, total))| ((city, total), 1))
        .collect()
}

fn apply(
    view: &mut Revenue<Tail>,
    orders: &mut ZSet<Order>,
    customers: &mut ZSet<Customer>,
    acc: &mut ZSet<(City, Cents)>,
    d_orders: ZSet<Order>,
    d_customers: ZSet<Customer>,
) -> ZSet<(City, Cents)> {
    let delta = view.step(d_orders.clone(), d_customers.clone());
    *acc = acc.clone() + delta.clone();
    *orders = orders.clone() + d_orders;
    *customers = customers.clone() + d_customers;
    assert_eq!(*acc, revenue_reference(orders, customers));
    delta
}

#[test]
fn revenue_per_city_matches_a_full_recompute_including_a_customer_move() {
    let mut view = Revenue::new();
    let mut orders = ZSet::new();
    let mut customers = ZSet::new();
    let mut acc = ZSet::new();

    let _ = apply(
        &mut view,
        &mut orders,
        &mut customers,
        &mut acc,
        ZSet::new(),
        rows([((1, "Wien"), 1), ((2, "Lyon"), 1)]),
    );
    let _ = apply(
        &mut view,
        &mut orders,
        &mut customers,
        &mut acc,
        rows([((1, (10, 800)), 1), ((1, (11, 80)), 1), ((2, (20, 500)), 1)]),
        ZSet::new(),
    );
    assert_eq!(acc, rows([(("Wien", 800), 1), (("Lyon", 500), 1)]));

    let moved = apply(
        &mut view,
        &mut orders,
        &mut customers,
        &mut acc,
        ZSet::new(),
        rows([((1, "Wien"), -1), ((1, "Graz"), 1)]),
    );
    assert_eq!(moved, rows([(("Wien", 800), -1), (("Graz", 800), 1)]));
    assert_eq!(acc, rows([(("Graz", 800), 1), (("Lyon", 500), 1)]));
}

#[test]
fn an_empty_change_into_the_view_emits_nothing() {
    let mut view = Revenue::new();
    let _ = view.step(rows([((1, (10, 800)), 1)]), rows([((1, "Wien"), 1)]));
    assert!(view.step(ZSet::new(), ZSet::new()).is_empty());
}
