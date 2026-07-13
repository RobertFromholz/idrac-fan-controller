use std::collections::HashMap;
use std::process::{exit, Command};
use std::thread;
use std::time::Duration;

const DEFAULT_DELAY: Duration = Duration::from_secs(10);
const DEFAULT_MAX_TEMPERATURE: u32 = 50;
const DEFAULT_MANUAL_FAN_SPEED: u8 = 5;

fn main() {
    let manual_fan_speed = std::env::var("MANUAL_FAN_SPEED").ok()
        .map(|value| value.parse::<u8>().expect("couldn't parse MANUAL_FAN_SPEED"))
        .unwrap_or(DEFAULT_MANUAL_FAN_SPEED);

    if manual_fan_speed > 100 {
        panic!("MANUAL_FAN_SPEED must be between 0 and 100");
    }

    let max_temperature = std::env::var("MAX_TEMPERATURE").ok()
        .map(|value| value.parse::<u32>().expect("couldn't parse MAX_TEMPERATURE"))
        .unwrap_or(DEFAULT_MAX_TEMPERATURE);

    let delay = std::env::var("DELAY").ok()
        .map(|value| value.parse::<u64>().expect("couldn't parse DELAY"))
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
                        is_manual_fan_control = true;
                    }
                    match set_manual_fan_speed(manual_fan_speed) {
                        Ok(_) => {}
                        Err(error) => {
                            eprintln!("Error setting manual fan speed: {}", error);
                            disable_manual_fan_control();
                            exit(1);
                        }
                    }
                }
            }
            Err(error) => {
                eprintln!("Error getting temperature: {error:?}");
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

fn set_manual_fan_speed(percentage: u8) -> Result<(), String> {
    if percentage > 100 {
        panic!("fan speed must be between 0 and 100");
    }
    println!("Setting manual fan speed to {}%", percentage);
    raw_ipmitool(format!("0x30 0x30 0x02 0xff {:#04x}", percentage))
}

fn raw_ipmitool(arguments: impl Into<String>) -> Result<(), String> {
    let output = Command::new("ipmitool")
        .arg("-I").arg("open")
        .arg("raw")
        .args(arguments.into().split(" "))
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
        .arg("-I").arg("open")
        .args(&["sdr", "type", "temperature"])
        .output()
        .expect("couldn't invoke ipmitool");
    if output.status.success() {
        let output = String::from_utf8(output.stdout)
            .map_err(|e| format!("couldn't parse ipmitool output: {}", e))?;
        let mut temperatures = HashMap::new();
        for line in output.lines() {
            let parts = line.split("|")
                .collect::<Vec<_>>();
            if parts.len() != 5 {
                continue;
            }
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