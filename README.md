# iDRAC Fan Controller

Script to manually keep fan speeds at a set value as long as temperatures are below a configured threshold.

The script will poll for the current temperature. As long as no sensor is above the configured threshold,
the script manually sets the fan speed to a specific value.

The purpose is to decrease noise and power consumption when the system is idle. As soon as the system heats up
the script re-enables iDRAC's fan controller. When the system cools down the script activates and manually set's
the fan speed.

## Usage

To use the script, set the below environment variables:

`MAX_TEMPERATURE` (default 50): the temperature (in degrees celcius) at which we give back responsibility 
to iDRAC to manage fan speeds.

`MANUAL_FAN_SPEED` (default 5%): the fan speed (in percent 0–100) to use when the script manages fan speeds.

`DELAY` (default 10s): the delay in seconds the script will poll for the current temperature. 