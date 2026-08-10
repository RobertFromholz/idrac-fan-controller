# iDRAC Fan Controller

> [!NOTE]  
> This program is not supported by newer versions of iDRAC, refer to [Supported iDRAC Versions](#supported-idrac-versions).

Program to manually control fan speeds on a PowerEdge/iDRAC server.

The program regularly polls for the system temperature and adjusts fan speeds accordingly.

The program aims to decrease noise as-well as power consumption when the system is close to idle.
When the server heats up it will re-activate iDRAC's own fan controller.
After the server returns to idle, the program will disable iDRAC's fan controller and manually lower fan speeds.

From limited testing it appears that (minimum) fan speeds are primarily determined by the ambient intake temperature.
As-such, if the server is mostly idle, iDRAC will force a fan speed that is unnecessarily high. 
The script can, for example, decrease fan speeds from 35-40% to 10-20% at the cost (?) of increasing CPU temperatures 
from 40˚C to 50˚C - drastically decreasing noise. 

The program aims to be as reliable as possible. In any case, if the program crashes or stops it will always attempt to reactivate 
iDRAC's own fan controller (see `ExecStopPost` in `idrac-fan-controller.service`).

## Usage

The program needs to be run as `root` directly on the host OS. It can't (for now) run inside a VM or control a remote iDRAC.

To use the program, you will need to install [Rust](https://rust-lang.org/tools/install/).

```shell
# Clone this repository.
git clone https://github.com/RobertFromholz/idrac-fan-controller.git

cd ./idrac-fan-controller

# Build the program.
cargo build --release

# 'Install' the program
sudo cp ./target/release/idrac-fan-controller /usr/local/bin/

# Copy the service file.
# You might also want to configure it.
sudo cp ./idrac-fan-controller.service /etc/systemd/system/

sudo systemctl daemon-reload
# Auto-start the service.
sudo systemctl enable idrac-fan-controller.service
# Start the service.
sudo systemctl start idrac-fan-controller.service
```

## Configuration

The program accepts a 'fan-curve' as an argument. If the program isn't given any arguments it does nothing: it always re-activates iDRAC's fan controller.

A 'fan-curve' is a list of arguments of the form `<max temperature>:<fan speed>`.

By default, `idrac-fan-controller.service` calls: `idrac-fan-controller 50:10 60:20`.

This means that if the temperature is:
* below 50˚C: the fan speed is set to 10%.
* between 50˚C and 60˚C: the fan speed is set to 20%.
* above 60˚C: iDRAC's fan controller is activated.

Specifically, the program will attempt to find the entry with the lowest max temperature that is still above the measured temperature, 
and set the fan speed according to that entry.

If an entry isn't found, that is if the temperature is higher than any entry, iDRAC's fan controller is activated.

To avoid the program oscillating between two fan speeds. The fan speed will not be decreased until the temperature drops 
3 degrees below the required temperature for a given fan speed. The exact temperature offset required can be configured 
by the `RAMP_DOWN_THRESHOLD` environment variable (by default: 3). 

The program polls iDRAC at regular intervals for the system's current temperature. The program will use the sensor with the highest measured
temperature to determine the appropriate fan speed.

The interval can be configured in seconds by setting the `DELAY` environment variable (by default: 10).

## Supported iDRAC versions

This program uses the `ipmitool` to send raw fan commands to iDRAC.

These commands are supported by all versions of iDRAC 7 and 8.

These commands are supported by versions up to and including 3.30.30.30 of iDRAC 9. After this release, trying to send such a command fails with: `insufficient privilege level` ([source](https://www.dell.com/community/en/conversations/poweredge-hardware-general/dell-eng-is-taking-away-fan-speed-control-away-from-users-idrac-3343434/647f8593f4ccf8a8de47aa9b)).

If you have upgraded past 3.30.30.30, you must downgrade iDRAC. 
Due to modifications to the iDRAC bootloader, you must downgrade to 4.40.10.00, then 4.10.10.00 and lastly 3.30.30.30.

Note: if you have at any point upgraded to 7.00.00.172 or newer, you will not be able to downgrade from 4.40.10.00 to 4.10.10.00 ([source](https://www.dell.com/support/kbdoc/en-pa/000225924/rac0181-idrac9-firmware-downgrade-failures-on-14-15g-poweredge-servers)).
In this case, you won't be able to use this program.

## Raw Commands

Below is a short summary of all `ipmitool` commands used by this program and their meaning.

All commands begin with `ipmitool -I open`. The interface `open` signifies that we want to communicate with a local iDRAC. `ipmitool` also supports communicating with remote hosts using the `lanplus` interface, however this is for the time being unsupported by this program.

`ipmitool -I open raw 0x30 0x30 0x01 0x00`: enable manual fan control. This disables iDRAC's fan controller and let's us control fan
speeds manually.

`ipmitool -I open raw 0x30 0x30 0x01 0x001`: disable manual fan control. This re-activates iDRAC's fan controller.

`ipmitool -I open raw 0x30 0x30 0x02 0xff 0x0A`: set manual fan speed. This is only available if we have already enabled manual fan control.
The last argument specifies the fan speed. The fan speed is specified as a percentage encoded as a 2-digit hexadecimal value.
In this case we set the fan speed to 10% (0x0A).

`ipmitool -I open sdr type temperature`: get system temperature. This returns the temperatures of all iDRAC temperature sensors.
```
Temp             | 01h | ok  |  3.1 | 50 degrees C
Temp             | 02h | ok  |  3.2 | 50 degrees C
Inlet Temp       | 05h | ok  |  7.1 | 31 degrees C
GPU1 Temp        | 89h | ns  |  7.1 | Disabled
GPU2 Temp        | 8Ah | ns  |  7.1 | Disabled
GPU3 Temp        | 62h | ns  |  7.1 | Disabled
GPU4 Temp        | 63h | ns  |  7.1 | Disabled
GPU5 Temp        | 64h | ns  |  7.1 | Disabled
GPU6 Temp        | 65h | ns  |  7.1 | Disabled
GPU7 Temp        | FCh | ns  |  7.1 | Disabled
GPU8 Temp        | FDh | ns  |  7.1 | Disabled
Exhaust Temp     | 06h | ok  |  7.1 | 40 degrees C
```

## Metrics

If the environment variable `METRICS_ADDRESS` is set, this program will expose metrics at the configured interface.

* `idrac_fan_controller_speed`: Fan speed as a percent from 0 or 100, or -1 if manual fan speed is disabled.
* `idrac_fan_controller_status`: Is manual fan speed enabled (1 = yes, 0 = no).
