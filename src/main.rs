use std::collections::{HashMap, VecDeque};
use std::fmt::Formatter;
use std::process::Command;
use std::time::Duration;
use std::{fmt, thread};

const ROLLING_AVERAGE_WINDOW: usize = 5;
const DEFAULT_DELAY: Duration = Duration::from_secs(10);

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
struct Temperature(u32);

impl fmt::Display for Temperature {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}˚C", self.0)
    }
}

#[derive(Debug, Copy, Clone, Ord, PartialOrd, PartialEq, Eq)]
struct FanSpeed(u8);

impl FanSpeed {
    pub fn new(percentage: u8) -> FanSpeed {
        if percentage > 100 {
            panic!("percentage must be between 0 and 100");
        }
        FanSpeed(percentage)
    }
}

impl fmt::LowerHex for FanSpeed {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for FanSpeed {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}%", self.0)
    }
}

#[derive(Debug)]
struct FanCurve {
    config: Vec<(Temperature, FanSpeed)>,
}

impl FanCurve {
    pub fn new(config: impl Into<Vec<(Temperature, FanSpeed)>>) -> FanCurve {
        let mut config = config.into();
        config.sort_by_key(|&(max_temp, _)| max_temp);
        FanCurve { config }
    }

    pub fn from_iter(iter: impl IntoIterator<Item = String>) -> FanCurve {
        let config = iter
            .into_iter()
            .map(|value| {
                let parts = value.split(":").collect::<Vec<_>>();
                assert_eq!(parts.len(), 2);
                let max_temp = parts[0]
                    .parse::<u32>()
                    .expect("couldn't parse max temperature");
                let fan_speed = parts[1].parse::<u8>().expect("couldn't parse fan speed");
                (Temperature(max_temp), FanSpeed::new(fan_speed))
            })
            .collect::<Vec<_>>();
        FanCurve::new(config)
    }

    /// The configured fan speed for the current temperature.
    /// Returns `None` if manual fan control should be disabled.
    ///
    /// # Examples
    ///
    /// ```
    /// use idrac-fan-controller::FanConfig;
    ///
    /// let config = FanConfig::new(&[
    ///     (Temperature(50), FanSpeed::new(10)),
    ///     (Temperature(60), FanSpeed::new(20))
    /// ]);
    ///
    /// assert_eq!(config.fan_speed(Temperature(45)), Some(FanSpeed::new(10)));
    /// assert_eq!(config.fan_speed(Temperature(55)), Some(FanSpeed::new(20)));
    /// assert_eq!(config.fan_speed(Temperature(65)), None));
    /// ```
    pub fn fan_speed(&self, temperature: Temperature) -> Option<FanSpeed> {
        for &(max_temp, fan_speed) in &self.config {
            if temperature < max_temp {
                return Some(fan_speed);
            }
        }
        None
    }
}

fn main() {
    // Load environment variables and the 'fan-curve'.
    let delay = std::env::var("DELAY")
        .ok()
        .map(|value| value.parse::<u64>().expect("couldn't parse DELAY"))
        .map(|delay| Duration::from_secs(delay))
        .unwrap_or(DEFAULT_DELAY);

    let config = FanCurve::from_iter(std::env::args().skip(1));

    let mut idrac = Idrac::new();

    let mut rolling_window = VecDeque::with_capacity(ROLLING_AVERAGE_WINDOW);

    loop {
        // The program loop.
        // 1) Retrieve the system temperature.
        // 2) Configure the system's fan speed as desired.
        // 3) Wait
        // 4) Repeat
        match get_max_temperature() {
            Ok(last_temperature) => {
                // Keep a rolling average of the last N temperatures.
                if rolling_window.len() == ROLLING_AVERAGE_WINDOW {
                    rolling_window.pop_front();
                }
                rolling_window.push_back(last_temperature);
                // Calculate the rolling average.
                let temperature = rolling_window.iter().sum::<u32>() / (rolling_window.len() as u32);

                let fan_speed = config.fan_speed(Temperature(temperature));
                match fan_speed {
                    None => {
                        if idrac.fan_speed != None {
                            println!("Temperature: {}", temperature);
                        }
                        if let Err(msg) = idrac.disable_manual_fan_control() {
                            eprintln!("Error: {}", msg);
                        }
                    }
                    Some(fan_speed) => {
                        if Some(fan_speed) != idrac.fan_speed {
                            println!("Temperature: {}", temperature);
                        }
                        if let Err(msg) = idrac.set_manual_fan_speed(fan_speed) {
                            eprintln!("Error: {}", msg);
                        }
                    }
                }
            }
            Err(msg) => {
                eprintln!("Error: {}", msg);
                let _ = idrac.disable_manual_fan_control();
            }
        }
        thread::sleep(delay);
    }
}

/// An object used to keep track of iDRAC's current fan speed, if any.
///
/// It also guarantees we at least try to re-active iDRAC's own fan-controller if the program
/// stops. It does this by implementing the `Drop` trait, which is called when the object goes out
/// of scope: if we either panic or exit.
struct Idrac {
    /// The current fan speed, if enabled.
    /// `None` if manual fan control is disabled.
    fan_speed: Option<FanSpeed>,
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
            .map_err(|msg| format!("couldn't disable manual fan control: {msg}"))?;
        self.fan_speed = None;
        Ok(())
    }

    pub fn set_manual_fan_speed(&mut self, percentage: FanSpeed) -> Result<(), String> {
        if self.fan_speed == Some(percentage) {
            return Ok(());
        }
        if self.fan_speed.is_none() {
            Idrac::enable_manual_fan_control()?;
        }
        println!("Setting manual fan speed to {}", percentage);
        raw_ipmitool(format!("0x30 0x30 0x02 0xff {:#04x}", percentage))
            .map_err(|msg| format!("couldn't set manual fan speed: {msg}"))?;
        self.fan_speed = Some(percentage);
        Ok(())
    }
}

impl Drop for Idrac {
    fn drop(&mut self) {
        let _ = self.disable_manual_fan_control();
    }
}

fn raw_ipmitool(arguments: impl Into<String>) -> Result<(), String> {
    // TODO: Let us configure our own login interface/arguments.
    //  Right now, we hard-code the 'open' interface. In the future, we might want
    //  to be able to control a remote iDRAC server.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fan_config_from_empty_iter() {
        let config = FanCurve::from_iter(Vec::<String>::new());
        assert_eq!(Vec::<(Temperature, FanSpeed)>::new(), config.config)
    }

    #[test]
    fn test_fan_config_from_iter() {
        let config = FanCurve::from_iter(["50:10".to_owned(), "60:20".to_owned()]);
        assert_eq!(vec![
            (Temperature(50), FanSpeed::new(10)),
            (Temperature(60), FanSpeed::new(20))
        ], config.config)
    }

    #[test]
    fn test_fan_config_from_unsorted_iter() {
        let config = FanCurve::from_iter(["60:20".to_owned(), "50:10".to_owned()]);
        assert_eq!(vec![
            (Temperature(50), FanSpeed::new(10)),
            (Temperature(60), FanSpeed::new(20))
        ], config.config)
    }

    #[test]
    fn test_empty_fan_config() {
        let config = FanCurve::new(&[]);
        assert_eq!(config.fan_speed(Temperature(0)), None);
        assert_eq!(config.fan_speed(Temperature(100)), None);
    }

    #[test]
    fn test_simple_fan_config() {
        let config = FanCurve::new(&[(Temperature(50), FanSpeed::new(10))]);
        assert_eq!(config.fan_speed(Temperature(45)), Some(FanSpeed::new(10)));
        assert_eq!(config.fan_speed(Temperature(50)), None);
        assert_eq!(config.fan_speed(Temperature(55)), None);
    }

    #[test]
    fn test_fan_config() {
        let config = FanCurve::new(&[
            (Temperature(50), FanSpeed::new(10)),
            (Temperature(60), FanSpeed::new(20)),
        ]);
        assert_eq!(config.fan_speed(Temperature(45)), Some(FanSpeed::new(10)));
        assert_eq!(config.fan_speed(Temperature(50)), Some(FanSpeed::new(20)));
        assert_eq!(config.fan_speed(Temperature(55)), Some(FanSpeed::new(20)));
        assert_eq!(config.fan_speed(Temperature(60)), None);
        assert_eq!(config.fan_speed(Temperature(65)), None);
    }

    #[test]
    fn test_unsorted_fan_config() {
        let config = FanCurve::new(&[
            (Temperature(60), FanSpeed::new(20)),
            (Temperature(50), FanSpeed::new(10)),
        ]);
        assert_eq!(config.fan_speed(Temperature(45)), Some(FanSpeed::new(10)));
        assert_eq!(config.fan_speed(Temperature(50)), Some(FanSpeed::new(20)));
        assert_eq!(config.fan_speed(Temperature(55)), Some(FanSpeed::new(20)));
        assert_eq!(config.fan_speed(Temperature(60)), None);
        assert_eq!(config.fan_speed(Temperature(65)), None);
    }
}
