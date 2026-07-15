use std::collections::HashMap;
use std::process::Command;
use std::thread;
use std::time::Duration;

const DEFAULT_DELAY: Duration = Duration::from_secs(10);
const DEFAULT_MAX_TEMPERATURE: u32 = 50;
const DEFAULT_MANUAL_FAN_SPEED: u8 = 5;

fn main() {
    let manual_fan_speed = std::env::var("MANUAL_FAN_SPEED")
        .ok()
        .map(|value| {
            value
                .parse::<u8>()
                .expect("couldn't parse MANUAL_FAN_SPEED")
        })
        .unwrap_or(DEFAULT_MANUAL_FAN_SPEED);

    if manual_fan_speed > 100 {
        panic!("MANUAL_FAN_SPEED must be between 0 and 100");
    }

    let max_temperature = std::env::var("MAX_TEMPERATURE")
        .ok()
        .map(|value| {
            value
                .parse::<u32>()
                .expect("couldn't parse MAX_TEMPERATURE")
        })
        .unwrap_or(DEFAULT_MAX_TEMPERATURE);

    let delay = std::env::var("DELAY")
        .ok()
        .map(|value| value.parse::<u64>().expect("couldn't parse DELAY"))
        .map(|delay| Duration::from_secs(delay))
        .unwrap_or(DEFAULT_DELAY);

    let mut idrac = Idrac::new();

    loop {
        let temperature = get_max_temperature().expect("couldn't get maximum temperature");
        if temperature > max_temperature {
            println!("Temperature: {}", temperature);
            idrac.disable_manual_fan_control();
        } else {
            idrac.set_manual_fan_speed(manual_fan_speed);
        }
        thread::sleep(delay);
    }
}

struct Idrac {
    /// The current fan speed, if enabled.
    /// `None` if manual fan control is disabled.
    fan_speed: Option<u8>,
}

impl Idrac {
    pub fn new() -> Idrac {
        Idrac { fan_speed: None }
    }

    fn enable_manual_fan_control() -> Result<(), String> {
        println!("Enabling manual fan control");
        raw_ipmitool("0x30 0x30 0x01 0x00")
            .map_err(|msg| format!("couldn't enable manual fan control: {msg}"))
    }

    pub fn disable_manual_fan_control(&mut self) -> Result<(), String> {
        println!("Disabling manual fan control");
        raw_ipmitool("0x30 0x30 0x01 0x01")
            .map_err(|msg | format!("couldn't disable manual fan control: {msg}"))?;
        self.fan_speed = None;
        Ok(())
    }

    pub fn set_manual_fan_speed(&mut self, percentage: u8) -> Result<(), String> {
        if percentage > 100 {
            return Err("percentage must be between 0 and 100".to_owned());
        }
        if self.fan_speed == Some(percentage) {
            return Ok(());
        }
        if self.fan_speed.is_none() {
            Idrac::enable_manual_fan_control()?;
        }
        println!("Setting manual fan speed to {}%", percentage);
        raw_ipmitool(format!("0x30 0x30 0x02 0xff {:#04x}", percentage))
            .map_err(|msg| format!("couldn't set manual fan speed: {msg}"))?;
        self.fan_speed = Some(percentage);
        Ok(())
    }
}

fn raw_ipmitool(arguments: impl Into<String>) -> Result<(), String> {
    let output = Command::new("ipmitool")
        .arg("-I")
        .arg("open")
        .arg("raw")
        .args(arguments.into().split(" "))
        .output()
        .map_err(|msg| format!("couldn't invoke ipmitool: {msg}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(message.into())
    }
}

fn get_max_temperature() -> Result<u32, String> {
    let temperatures = get_temperatures()?;
    temperatures
        .into_values()
        .max()
        .ok_or_else(|| "couldn't find temperature".to_owned())
}

fn get_temperatures() -> Result<HashMap<String, u32>, String> {
    let output = Command::new("ipmitool")
        .arg("-I")
        .arg("open")
        .args(&["sdr", "type", "temperature"])
        .output()
        .map_err(|msg| format!("couldn't invoke ipmitool: {msg}"))?;
    if output.status.success() {
        let output = String::from_utf8(output.stdout)
            .map_err(|e| format!("couldn't parse ipmitool output: {}", e))?;
        let mut temperatures = HashMap::new();
        for line in output.lines() {
            let parts = line.split("|").collect::<Vec<_>>();
            if parts.len() != 5 {
                continue;
            }
            let name = parts[0].trim();
            let temperature = parts[4].trim();
            if temperature == "No Reading" {
                continue;
            }
            let temperature = temperature
                .split(" ")
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
