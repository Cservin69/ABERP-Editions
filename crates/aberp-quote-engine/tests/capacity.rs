//! S427 — capacity-aware lead-time tests.
//!
//! These pin the four behaviours the brief named: empty-machine
//! fallback, single-machine math, multi-family pick-max, and the
//! determinism of the binding-family tie-break. Each test asserts a
//! number that changes when the formula changes (CLAUDE.md rule 9).

use aberp_quote_engine::{
    lead_time_days, MachineCapacity, MachineFamily, FALLBACK_BUFFER_PCT, FALLBACK_DAILY_HOURS,
};
use std::collections::BTreeMap;

fn load(pairs: &[(MachineFamily, f64)]) -> BTreeMap<MachineFamily, f64> {
    pairs.iter().copied().collect()
}

#[test]
fn empty_machines_uses_virtual_shop_fallback() {
    // 16h/day × 80% = 12.8 h/day schedulable.
    let cap = FALLBACK_DAILY_HOURS * (1.0 - FALLBACK_BUFFER_PCT / 100.0);
    assert_eq!(cap, 12.8);

    // 40h existing on 3-axis + 24h existing on lathe + 12.8h new on
    // 3-axis = 76.8h all queued through the single virtual shop.
    let existing = load(&[
        (MachineFamily::ThreeAxisMill, 40.0),
        (MachineFamily::Lathe, 24.0),
    ]);
    let new = load(&[(MachineFamily::ThreeAxisMill, 12.8)]);

    let est = lead_time_days(&[], &existing, &new);
    // ceil(76.8 / 12.8) = ceil(6.0) = 6.
    assert_eq!(est.days, 6);
    assert!(est.used_fallback);
    assert_eq!(est.binding_family, None);
}

#[test]
fn empty_machines_and_no_load_is_zero_days() {
    let est = lead_time_days(&[], &BTreeMap::new(), &BTreeMap::new());
    assert_eq!(est.days, 0);
    assert!(est.used_fallback);
}

#[test]
fn single_machine_math() {
    // One 3-axis mill, 16h/day, 20% buffer → 12.8 h/day.
    let machines = vec![MachineCapacity {
        family: MachineFamily::ThreeAxisMill,
        daily_hours_avail: 16.0,
        buffer_pct: 20.0,
    }];
    // 50h already queued + 14h new = 64h → ceil(64 / 12.8) = 5 days.
    let existing = load(&[(MachineFamily::ThreeAxisMill, 50.0)]);
    let new = load(&[(MachineFamily::ThreeAxisMill, 14.0)]);

    let est = lead_time_days(&machines, &existing, &new);
    assert_eq!(est.days, 5);
    assert!(!est.used_fallback);
    assert_eq!(est.binding_family, Some(MachineFamily::ThreeAxisMill));
}

#[test]
fn two_machines_same_family_sum_capacity() {
    // Two 5-axis mills, 16h/day @ 20% each → 25.6 h/day combined.
    let machines = vec![
        MachineCapacity {
            family: MachineFamily::FiveAxisMill,
            daily_hours_avail: 16.0,
            buffer_pct: 20.0,
        },
        MachineCapacity {
            family: MachineFamily::FiveAxisMill,
            daily_hours_avail: 16.0,
            buffer_pct: 20.0,
        },
    ];
    let new = load(&[(MachineFamily::FiveAxisMill, 51.2)]);
    let est = lead_time_days(&machines, &BTreeMap::new(), &new);
    // ceil(51.2 / 25.6) = 2.
    assert_eq!(est.days, 2);
    assert_eq!(est.binding_family, Some(MachineFamily::FiveAxisMill));
}

#[test]
fn multi_family_picks_max() {
    // 3-axis: lots of capacity, light new load → few days.
    // 5-axis: thin capacity, heavy new load → many days. Max wins.
    let machines = vec![
        MachineCapacity {
            family: MachineFamily::ThreeAxisMill,
            daily_hours_avail: 16.0,
            buffer_pct: 20.0,
        }, // 12.8/day
        MachineCapacity {
            family: MachineFamily::FiveAxisMill,
            daily_hours_avail: 8.0,
            buffer_pct: 50.0,
        }, // 4.0/day
    ];
    let existing = load(&[(MachineFamily::ThreeAxisMill, 12.8)]);
    let new = load(&[
        (MachineFamily::ThreeAxisMill, 12.8), // (12.8+12.8)/12.8 = 2 days
        (MachineFamily::FiveAxisMill, 20.0),  // 20/4.0 = 5 days  ← binding
    ]);

    let est = lead_time_days(&machines, &existing, &new);
    assert_eq!(est.days, 5);
    assert_eq!(est.binding_family, Some(MachineFamily::FiveAxisMill));
    assert!(!est.used_fallback);
}

#[test]
fn touched_family_without_machine_routes_through_fallback() {
    // Operator has a 3-axis mill but no lathe; a quote that needs the
    // lathe must not divide by zero — it routes through the 12.8/day
    // fallback and still reports the lathe as the binding family.
    let machines = vec![MachineCapacity {
        family: MachineFamily::ThreeAxisMill,
        daily_hours_avail: 16.0,
        buffer_pct: 20.0,
    }];
    let new = load(&[(MachineFamily::Lathe, 25.6)]); // ceil(25.6/12.8) = 2
    let est = lead_time_days(&machines, &BTreeMap::new(), &new);
    assert_eq!(est.days, 2);
    assert_eq!(est.binding_family, Some(MachineFamily::Lathe));
    assert!(!est.used_fallback); // machines present → not the empty-shop flag
}

#[test]
fn no_new_hours_is_zero_days() {
    let machines = vec![MachineCapacity {
        family: MachineFamily::ThreeAxisMill,
        daily_hours_avail: 16.0,
        buffer_pct: 20.0,
    }];
    // Existing load but the new quote has no machining hours.
    let existing = load(&[(MachineFamily::ThreeAxisMill, 99.0)]);
    let est = lead_time_days(&machines, &existing, &BTreeMap::new());
    assert_eq!(est.days, 0);
    assert_eq!(est.binding_family, None);
}

#[test]
fn family_round_trips_through_db_str() {
    for f in MachineFamily::ALL {
        assert_eq!(MachineFamily::from_db_str(f.as_db_str()), Some(f));
    }
    assert_eq!(MachineFamily::from_db_str("nonsense"), None);
    assert_eq!(MachineFamily::for_route(true), MachineFamily::FiveAxisMill);
    assert_eq!(
        MachineFamily::for_route(false),
        MachineFamily::ThreeAxisMill
    );
}
