//! Предупреждение о месте — до того, как оно кончилось.
//!
//! Отказ в записи на последнем мегабайте — плохой способ узнать о квоте: к
//! этому моменту мир уже не сохраняется. Поэтому демон говорит мастеру
//! заранее, когда занятое переходит порог, и ещё раз, когда опускается
//! обратно. Между порогами — промежуток: место, которое колеблется у самой
//! черты, иначе давало бы предупреждение на каждом втором замере.

use schema::noded::{PRESSURE_CLEAR_PERCENT, PRESSURE_WARN_PERCENT};

/// Новое состояние предупреждения, если оно сменилось.
///
/// Без лимита предупреждать не о чем, и поднятое прежде снимается.
pub fn step(raised: bool, used: u64, limit: u64) -> Option<bool> {
    if limit == 0 {
        return raised.then_some(false);
    }
    // В u128: умноженное на сто число байт не должно упираться в потолок u64.
    let percent = u128::from(used) * 100 / u128::from(limit);
    if !raised && percent >= u128::from(PRESSURE_WARN_PERCENT) {
        Some(true)
    } else if raised && percent < u128::from(PRESSURE_CLEAR_PERCENT) {
        Some(false)
    } else {
        None
    }
}

/// Сколько места на диске ноды можно занять, пока не тронут её запас, и
/// сколько уже занято — в тех же единицах, что у `NodeStats`.
pub fn node_usage(disk_used_mb: i64, disk_total_mb: i64, reserve_mb: u64) -> (u64, u64) {
    let total = disk_total_mb.max(0) as u64;
    (disk_used_mb.max(0) as u64, total.saturating_sub(reserve_mb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_warning_is_raised_once_and_withdrawn_below_the_gap() {
        assert_eq!(step(false, 89, 100), None);
        assert_eq!(step(false, 90, 100), Some(true));
        assert_eq!(step(true, 95, 100), None, "уже поднято — молчим");
        assert_eq!(step(true, 87, 100), None, "между порогами — молчим");
        assert_eq!(step(true, 84, 100), Some(false));
        assert_eq!(step(false, 87, 100), None);
    }

    #[test]
    fn no_limit_means_no_warning() {
        assert_eq!(step(false, u64::MAX, 0), None);
        assert_eq!(step(true, 1, 0), Some(false));
    }

    #[test]
    fn huge_numbers_do_not_wrap() {
        assert_eq!(step(false, u64::MAX, u64::MAX), Some(true));
    }

    #[test]
    fn the_node_limit_leaves_out_the_reserve() {
        assert_eq!(node_usage(900, 1000, 100), (900, 900));
        assert_eq!(node_usage(10, 1000, 5000), (10, 0));
    }
}
