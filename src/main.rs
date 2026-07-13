use std::collections::HashMap;
use std::process::{exit, Command};
use std::thread;
use std::time::Duration;

const DEFAULT_DELAY: Duration = Duration::from_secs(10);
const DEFAULT_MAX_TEMPERATURE: u32 = 50;
const DEFAULT_MANUAL_FAN_SPEED: u8 = 5;

fn main() {
    let manual_fan_speed = std::env::var("MANUAL_FAN_SPEED").ok()
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(DEFAULT_MANUAL_FAN_SPEED);

    let max_temperature = std::env::var("MAX_TEMPERATURE").ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(DEFAULT_MAX_TEMPERATURE);

    let delay = std::env::var("DELAY").ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|delay| Duration::from_secs(delay))
        .unwrap_or(DEFAULT_DELAY);

    // Whether we have already enabled manual fan control.
    let mut is_manual_fan_control = false;

    loop {
        let temperature = get_max_temperature();
        match temperature {
            Ok(temperature) => {
                println!("Max temperature: {}", temperature);
                if temperature > max_temperature {
                    if is_manual_fan_control {
                        disable_manual_fan_control();
                        is_manual_fan_control = false;
                    }
                } else {
                    if !is_manual_fan_control {
                        enable_manual_fan_control();
                    }
                    set_manual_fan_speed(manual_fan_speed);
                }
            }
            Err(error) => {
                eprintln!("{error:?}");
                // Try to disable manual fan control.
                // We might not necessarily have encountered an error with ipmitool.
                disable_manual_fan_control();
                exit(1);
            }
        }
        thread::sleep(delay);
    }
}

fn enable_manual_fan_control() {
    println!("Enabling manual fan control");
    raw_ipmitool("0x30 0x30 0x01 0x00")
        .expect("couldn't enable manual fan control");
}

fn disable_manual_fan_control() {
    println!("Disabling manual fan control");
    raw_ipmitool("0x30 0x30 0x01 0x01")
        .expect("couldn't disable manual fan control");
}

fn set_manual_fan_speed(percentage: u8) {
    if percentage >= 100 {
        panic!("percentage must be between 0 and 100");
    }
    println!("Setting manual fan speed to {}%", percentage);
    raw_ipmitool(format!("0x30 0x30 0x02 0xff {:#04x}", percentage))
        .expect("couldn't set manual fan speed");
}

fn raw_ipmitool(arguments: impl Into<String>) -> Result<(), String> {
    let output = Command::new("ipmitool")
        .arg("-I").arg("open")
        .arg("raw")
        .arg(arguments.into())
        .output()
        .expect("couldn't invoke ipmitool");
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(message.into())
    }
}

fn get_max_temperature() -> Result<u32, String> {
    let temperatures = get_temperatures()?;
    temperatures.into_values()
        .max()
        .ok_or_else(|| "couldn't find any temperature".to_owned())
}

fn get_temperatures() -> Result<HashMap<String, u32>, String> {
    let output = Command::new("ipmitool")
        .arg("-i").arg("open")
        .args(&["sdr", "type", "temperature"])
        .output()
        .expect("couldn't invoke ipmitool");
    if output.status.success() {
        let output = String::from_utf8(output.stdout).unwrap();
        let mut temperatures = HashMap::new();
        for line in output.lines() {
            let parts = line.split("|")
                .collect::<Vec<_>>();
            let name = parts[0].trim();
            let temperature = parts[4].trim();
            if temperature == "No Reading" {
                continue;
            }
            let temperature = temperature.split(" ")
                .next()
                .and_then(|value| value.parse::<u32>().ok());
            if let Some(temperature) = temperature {
                temperatures.insert(name.to_owned(), temperature);
            };
        }
        Ok(temperatures)
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(message.into())
    }
}