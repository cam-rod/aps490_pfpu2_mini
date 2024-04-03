//! Basic component structs
use cortex_m::singleton;
use critical_section::CriticalSection;
use defmt::{debug, error, info, warn, Format, Formatter};
use embedded_hal::digital::{OutputPin, PinState};
use rp2040_hal::gpio::{
    bank0::{Gpio6, Gpio7, Gpio8},
    FunctionNull, FunctionSio, Pin, PullDown, SioOutput,
};

use crate::interrupt::{BUFFERS, STATUS_LEDS};

/// Index of a detection event, combined with voltage difference
pub type DetectionEvent = (SampleCounter, u8);

/// All states for LEDs
pub enum StatusLedStates {
    /// Green
    Normal,
    /// Yellow
    Alert,
    /// Red
    Error,
    /// None illuminated
    Disabled,
}

/// Controls the status LEDs on separate pins
pub struct StatusLedMulti {
    /// Current LED state
    pub state: StatusLedStates,
    normal_led: Pin<Gpio6, FunctionSio<SioOutput>, PullDown>,
    alert_led: Pin<Gpio7, FunctionSio<SioOutput>, PullDown>,
    error_led: Pin<Gpio8, FunctionSio<SioOutput>, PullDown>,
}

/// Init LED GPIO pins for hnadling via interrupt
impl StatusLedMulti {
    /// Panic message if no LEDs have been configured.
    pub const NO_LED_PANIC_MSG: &'static str =
        "Unable to display state due to non-configured LEDs, or not available in mutex";
    const RESET_MSG: &'static str = "\nSystem must be power cycled to restore normal operation.";

    pub fn init(
        normal_led: Pin<Gpio6, FunctionNull, PullDown>,
        alert_led: Pin<Gpio7, FunctionNull, PullDown>,
        error_led: Pin<Gpio8, FunctionNull, PullDown>,
    ) -> Option<&'static mut Self> {
        singleton!(: StatusLedMulti = Self {
            state: StatusLedStates::Alert,
            normal_led: normal_led.into_push_pull_output_in_state(PinState::Low),
            alert_led: alert_led.into_push_pull_output_in_state(PinState::High),
            error_led: error_led.into_push_pull_output_in_state(PinState::Low),
        })
    }

    /// Set [`StatusLedStates::Error`] within a [`CriticalSection`]
    pub fn set_error(cs: CriticalSection, message: Option<&str>) {
        let status = STATUS_LEDS.take(cs).expect(Self::NO_LED_PANIC_MSG);
        if let Some(msg_text) = message {
            error!(
                "Error encountered during operation:\n{=str}{=str}",
                msg_text,
                Self::RESET_MSG
            );
        } else {
            error!(
                "Unknown error encountered during operation.{=str}",
                Self::RESET_MSG
            );
        }

        match status.state {
            StatusLedStates::Normal => status.normal_led.set_low().unwrap(),
            StatusLedStates::Alert => status.alert_led.set_low().unwrap(),
            StatusLedStates::Error | StatusLedStates::Disabled => {}
        };
        status.error_led.set_high().unwrap();
        status.state = StatusLedStates::Error;
        STATUS_LEDS.replace(cs, Some(status));
    }

    /// Set [`StatusLedStates::Alert`] within a [`CriticalSection`]
    pub fn set_alert(cs: CriticalSection, message: Option<DetectionMsg>) {
        let status = STATUS_LEDS.take(cs).expect(Self::NO_LED_PANIC_MSG);
        if let Some(detection_msg) = message {
            info!("{}", detection_msg);
        } else {
            warn!("Unknown alert raised!");
        }

        match status.state {
            StatusLedStates::Normal => status.normal_led.set_low().unwrap(),
            StatusLedStates::Error => status.error_led.set_low().unwrap(),
            StatusLedStates::Alert | StatusLedStates::Disabled => {}
        };
        status.alert_led.set_high().unwrap();
        status.state = StatusLedStates::Alert;
        STATUS_LEDS.replace(cs, Some(status));
    }
}

/// Monotonic counter indicating the position of averaged samples.
#[derive(Default, Debug, Ord, PartialOrd, Eq, PartialEq, Copy, Clone)]
pub struct SampleCounter(usize);
impl SampleCounter {
    /// Get current counter value
    pub fn get_counter(&self) -> usize {
        self.0
    }

    /// Increment counter (mainly used by [`Buffers.current_sample`](Buffers). An error will be
    /// raised if any counter reaches [`u32::MAX`].
    pub fn increment(&mut self) {
        if self.0.checked_add(1).is_none() {
            critical_section::with(|cs| {
                debug!("critical_section: counter set_error overflow");
                StatusLedMulti::set_error(
                    cs,
                    Some("No ADC transfer in progress! Unable to collect latest readings"),
                )
            })
        }
    }

    /// Add with defined wrapping. Result will be within range \[0, `limit` - 1\].
    ///
    /// Used to index [`Buffers.longterm_buffer`](Buffers)
    pub fn wrapping_counter_add(&self, rhs: usize, limit: usize) -> usize {
        if self.0 + rhs >= limit {
            rhs - (limit - self.0)
        } else {
            self.0 + rhs
        }
    }

    /// Subtract with defined wrapping. Limit will be within range \[0, `limit` - 1\].
    ///
    /// Used to index [`Buffers.longterm_buffer`](Buffers)
    pub fn wrapping_counter_sub(&self, rhs: usize, limit: usize) -> usize {
        if self.0.wrapping_sub(rhs) >= limit {
            limit - (rhs - self.0)
        } else {
            self.0 - rhs
        }
    }
}

/// Various buffers used for managing signal samples
pub struct Buffers {
    /// Records up to 45k averaged samples (90 s with 2 ms averaging) to determine if a detection event occurred
    longterm_buffer: [u8; 45000],
    /// Counter for the most recent sample added to
    current_sample: SampleCounter,
    /// Rotates position time stamps for up to 10 recent detection events, comparable with `current_sample`.
    /// Most recent event is stored at index 0
    detection_events: [Option<DetectionEvent>; 10],
    /// A potential detection event has been recorded, and the system is awaiting a second average sample
    await_confirm: bool,
}

impl Buffers {
    /// Initial averaged difference used for detecting contact.
    ///
    /// Ex. a trigger delta of 128 on a 3.3V signal requires that the average voltage range has
    /// decreased by approximately 1.65V.
    const INIT_TRIGGER_DELTA: u8 = 160;
    /// Initial averaged difference to restore [`StatusLedStates::Normal`].
    ///
    /// This is the increase in voltage relative to the last detection event.
    const INIT_RESTORE_DELTA: u8 = 100;
    /// Panic message raised if buffers are not available
    pub const NO_BUFFER_PANIC_MSG: &'static str =
        "Buffers have not been initialized or are not currently available in mutex";

    /// Initialize [`BUFFERS`]
    pub fn init() {
        match singleton!(:Buffers = Self {
            longterm_buffer: [0u8; 45000],
            current_sample: SampleCounter::default(),
            detection_events: [None; 10],
            await_confirm: false
        }) {
            Some(init_buffers) => {
                debug!("critical_section: init buffers");
                critical_section::with(|cs| BUFFERS.replace(cs, Some(init_buffers)));
            }
            None => warn!("Buffers have already been initiated"),
        }
    }

    /// Insert a new sample at the head
    pub fn insert(&mut self, sample: u8) {
        let new_head = self
            .current_sample
            .wrapping_counter_add(1, self.longterm_buffer.len());
        self.longterm_buffer[new_head] = sample;
        self.current_sample.increment();
    }

    /// Analyze the most recent data to determine if a contact event has occurred.
    ///
    /// Also updates the record of recent detection events
    pub fn detect_contact(&mut self) {
        if !self.await_confirm {
            // First contact check
            let prev_sample = self
                .current_sample
                .wrapping_counter_sub(1, self.longterm_buffer.len());
            if self.longterm_buffer[prev_sample]
                - self.longterm_buffer[self.current_sample.get_counter()]
                >= Self::INIT_TRIGGER_DELTA
            {
                self.await_confirm = true;
            }
        } else {
            // Validation contact check
            let prev_high_sample = self
                .current_sample
                .wrapping_counter_sub(2, self.longterm_buffer.len());
            if self.longterm_buffer[prev_high_sample]
                - self.longterm_buffer[self.current_sample.get_counter()]
                >= Self::INIT_TRIGGER_DELTA
            {
                //
                critical_section::with(|cs| {
                    StatusLedMulti::set_alert(cs, Some(DetectionMsg::create(self)))
                });
                self.add_detection_event();
                self.await_confirm = false;
            }
        }
    }

    /// Shortcut to return index of a successful detection sample.
    ///
    ///```
    /// static BUFFERS = Buffers::init();
    ///
    /// BUFFERS.insert(12);
    /// assert_eq!(BUFFERS.detection_idx(), BUFFERS.current_sample.get_counter() - 1)
    ///```
    pub fn detection_idx(&self) -> usize {
        self.current_sample.get_counter() - 1
    }

    /// Add an entry to the `detection_events` array, based on the penultimate sample.
    fn add_detection_event(&mut self) {
        self.detection_events.rotate_right(1);
        self.detection_events[0] = Some((
            self.current_sample,
            self.longterm_buffer[self.current_sample.get_counter()],
        ));
    }

    /// Analyze the most recent data and contact events to determine when contact ends
    pub fn detect_end_contact(&mut self) -> bool {
        todo!()
    }
}

/// Newtype to send formatted error messages when [`Buffers::detect_contact`] is successful.
pub struct DetectionMsg(SampleCounter);
impl DetectionMsg {
    /// Create a detection message:
    ///
    /// > "contact detected on sample {[`Buffers::detection_idx`]}! Adding to detection events"`
    fn create(buffer: &Buffers) -> Self {
        Self(SampleCounter(buffer.detection_idx()))
    }
}
impl Format for DetectionMsg {
    fn format(&self, fmt: Formatter) {
        defmt::write!(
            fmt,
            "contact detected on sample {}! Adding to detection events",
            self.0.get_counter()
        )
    }
}

/// Creates a buffer for ADC DMA transfers
pub fn create_avg_buffer() -> Option<&'static mut [u8; 4000]> {
    singleton!(: [u8; 4000] = [0u8; 4000])
}
