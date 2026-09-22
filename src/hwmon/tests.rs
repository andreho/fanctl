//! Tests for the hwmon reader/writer against a **fake** `/sys/class/hwmon`
//! tree built in a temp dir, so no real fan ever gets touched.

use super::discover;
use std::fs;

/// Build a fake hwmon tree and return its base directory.
fn make_fake_chip() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let chip = dir.path().join("hwmon0");
    fs::create_dir_all(&chip).unwrap();
    fs::write(chip.join("name"), "it8792\n").unwrap();
    fs::write(chip.join("pwm1"), "182\n").unwrap();
    fs::write(chip.join("pwm1_enable"), "0\n").unwrap();
    fs::write(chip.join("pwm1_max"), "255\n").unwrap();
    fs::write(chip.join("pwm2"), "100\n").unwrap();
    fs::write(chip.join("pwm2_enable"), "0\n").unwrap();
    fs::write(chip.join("temp1_input"), "33000\n").unwrap();
    fs::write(chip.join("temp1_label"), "T1\n").unwrap();
    fs::write(chip.join("temp2_input"), "35000\n").unwrap();
    fs::write(chip.join("fan1_input"), "1650\n").unwrap();
    dir
}

#[test]
fn discovers_chip_and_sensors() {
    let dir = make_fake_chip();
    let chips = discover(dir.path());
    assert_eq!(chips.len(), 1, "chips: {:?}", chips);
    let c = &chips[0];
    assert_eq!(c.name, "it8792");
    assert_eq!(c.pwm_numbers(), vec![1, 2]);
    assert_eq!(c.temp_numbers(), vec![1, 2]);
    assert_eq!(c.fan_numbers(), vec![1]);
}

#[test]
fn reads_temps_and_pwm() {
    let dir = make_fake_chip();
    let c = &discover(dir.path())[0];
    assert_eq!(c.read_i64("temp1_input"), Some(33000));
    assert_eq!(c.read("temp1_label").as_deref(), Some("T1"));
    assert_eq!(c.pwm_raw(1), Some(182));
    assert_eq!(c.pwm_enable(1), Some(0));
    // 182/255 == 71.4%
    let pct = c.pwm_percent(1, 255).unwrap();
    assert!((pct - 71.37).abs() < 0.1, "pct={pct}");
}

#[test]
fn writes_pwm_manual_and_modes() {
    let dir = make_fake_chip();
    let c = &discover(dir.path())[0];
    c.set_pwm_manual(1, 50.0, 255).unwrap();
    assert_eq!(c.read("pwm1_enable").as_deref(), Some("1"));
    assert_eq!(c.pwm_raw(1), Some(128), "50% of 255 rounds to 128");

    // Off is a 0% manual duty: the it87 family's `pwmN_enable = 0` does not
    // stop the fan (the driver keeps it on in on/off mode at 100%).
    c.set_pwm_off(1, 255).unwrap();
    assert_eq!(c.read("pwm1_enable").as_deref(), Some("1"));
    assert_eq!(c.pwm_raw(1), Some(0));
    c.set_pwm_auto(1).unwrap();
    assert_eq!(c.read("pwm1_enable").as_deref(), Some("2"));
    c.set_pwm_full(1, 255).unwrap();
    // `Full` is manual mode at the chip's maximum.
    assert_eq!(c.read("pwm1_enable").as_deref(), Some("1"));
    assert_eq!(c.pwm_raw(1), Some(255));
}
