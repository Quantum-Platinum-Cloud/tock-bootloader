//! Tock kernel for the bootloader on nrf52 over CDC/USB.
//!
//! It is based on nRF52840 SoC (Cortex M4 core with a BLE + IEEE 802.15.4 transceiver).

#![no_std]
// Disable this attribute when documenting, as a workaround for
// https://github.com/rust-lang/rust/issues/62184.
#![cfg_attr(not(doc), no_main)]

use core::panic::PanicInfo;

use kernel::capabilities;
use kernel::common::dynamic_deferred_call::{DynamicDeferredCall, DynamicDeferredCallClientState};
use kernel::component::Component;
use kernel::hil;
use kernel::hil::time::Alarm;
use kernel::hil::time::Counter;
use kernel::hil::usb::Client;
use kernel::mpu::MPU;
use kernel::Chip;
#[allow(unused_imports)]
use kernel::{create_capability, debug, debug_gpio, debug_verbose, static_init};

use capsules::virtual_alarm::VirtualMuxAlarm;

use nrf52840::interrupt_service::Nrf52840DefaultPeripherals;

// use nrf52840::gpio::Pin;
// use nrf52_components::{self, UartChannel, UartPins};
// const LED_KERNEL_PIN: Pin = Pin::P0_13;
// const UART_RTS: Option<Pin> = Some(Pin::P0_05);
// const UART_TXD: Pin = Pin::P0_06;
// const UART_CTS: Option<Pin> = Some(Pin::P0_07);
// const UART_RXD: Pin = Pin::P0_08;

const LED_ON_PIN: nrf52840::gpio::Pin = nrf52840::gpio::Pin::P1_09;

include!(concat!(env!("OUT_DIR"), "/attributes.rs"));

// Number of concurrent processes this platform supports.
const NUM_PROCS: usize = 0;

static mut PROCESSES: [Option<&'static dyn kernel::procs::ProcessType>; NUM_PROCS] =
    [None; NUM_PROCS];

static mut CHIP: Option<&'static nrf52840::chip::NRF52<Nrf52840DefaultPeripherals>> = None;

/// Dummy buffer that causes the linker to reserve enough space for the stack.
#[no_mangle]
#[link_section = ".stack_buffer"]
pub static mut STACK_MEMORY: [u8; 0x2000] = [0; 0x2000];

/// Function to allow the bootloader to exit by reseting the chip.
fn bootloader_exit() {
    unsafe {
        cortexm4::scb::reset();
    }
}

/// Supported drivers by the platform
pub struct Platform {
    bootloader: &'static bootloader::bootloader::Bootloader<
        'static,
        bootloader::uart_receive_multiple_timeout::UartReceiveMultipleTimeout<
            'static,
            VirtualMuxAlarm<'static, nrf52::rtc::Rtc<'static>>,
        >,
        bootloader::flash_large_to_small::FlashLargeToSmall<'static, nrf52::nvmc::Nvmc>,
    >,
}

impl kernel::Platform for Platform {
    fn with_driver<F, R>(&self, _driver_num: usize, f: F) -> R
    where
        F: FnOnce(Option<&dyn kernel::Driver>) -> R,
    {
        f(None)
    }
}

/// Entry point in the vector table called on hard reset.
#[no_mangle]
pub unsafe fn reset_handler() {
    // Loads relocations and clears BSS
    nrf52840::init();
    let ppi = static_init!(nrf52840::ppi::Ppi, nrf52840::ppi::Ppi::new());
    // Initialize chip peripheral drivers
    let nrf52840_peripherals = static_init!(
        Nrf52840DefaultPeripherals,
        Nrf52840DefaultPeripherals::new(ppi)
    );

    // set up circular peripheral dependencies
    nrf52840_peripherals.init();
    let base_peripherals = &nrf52840_peripherals.nrf52;

    let board_kernel = static_init!(kernel::Kernel, kernel::Kernel::new(&PROCESSES));

    //--------------------------------------------------------------------------
    // BOOTLOADER ENTRY
    //--------------------------------------------------------------------------

    // Decide very early if we want to stay in the bootloader so we don't run a
    // bunch of init code just to reset into the kernel.

    let bootloader_entry_mode = static_init!(
        bootloader_nrf52::bootloader_entry_gpregret::BootloaderEntryGpRegRet,
        bootloader_nrf52::bootloader_entry_gpregret::BootloaderEntryGpRegRet::new(
            &base_peripherals.pwr_clk
        )
    );

    let bootloader_jumper = static_init!(
        bootloader_cortexm::jumper::CortexMJumper,
        bootloader_cortexm::jumper::CortexMJumper::new()
    );

    let active_notifier_led = static_init!(
        kernel::hil::led::LedHigh<'static, nrf52840::gpio::GPIOPin>,
        kernel::hil::led::LedHigh::new(&nrf52840_peripherals.gpio_port[LED_ON_PIN])
    );

    let bootloader_active_notifier = static_init!(
        bootloader::active_notifier_ledon::ActiveNotifierLedon,
        bootloader::active_notifier_ledon::ActiveNotifierLedon::new(active_notifier_led)
    );

    let bootloader_enterer = static_init!(
        bootloader::bootloader::BootloaderEnterer<'static>,
        bootloader::bootloader::BootloaderEnterer::new(
            bootloader_entry_mode,
            bootloader_jumper,
            bootloader_active_notifier
        )
    );

    // First decide if we want to actually run the bootloader or not.
    bootloader_enterer.check();

    //--------------------------------------------------------------------------
    // CAPABILITIES
    //--------------------------------------------------------------------------

    // Create capabilities that the board needs to call certain protected kernel
    // functions.
    let main_loop_capability = create_capability!(capabilities::MainLoopCapability);

    //--------------------------------------------------------------------------
    // Deferred Call (Dynamic) Setup
    //--------------------------------------------------------------------------

    let dynamic_deferred_call_clients =
        static_init!([DynamicDeferredCallClientState; 2], Default::default());
    let dynamic_deferred_caller = static_init!(
        DynamicDeferredCall,
        DynamicDeferredCall::new(dynamic_deferred_call_clients)
    );
    DynamicDeferredCall::set_global_instance(dynamic_deferred_caller);

    //--------------------------------------------------------------------------
    // ALARM & TIMER
    //--------------------------------------------------------------------------

    let rtc = &base_peripherals.rtc;
    rtc.start();

    let mux_alarm = components::alarm::AlarmMuxComponent::new(rtc)
        .finalize(components::alarm_mux_component_helper!(nrf52::rtc::Rtc));

    // //--------------------------------------------------------------------------
    // // UART DEBUGGING
    // //--------------------------------------------------------------------------

    // let channel = nrf52_components::UartChannelComponent::new(
    //     UartChannel::Pins(UartPins::new(UART_RTS, UART_TXD, UART_CTS, UART_RXD)),
    //     mux_alarm,
    //     &base_peripherals.uarte0,
    // )
    // .finalize(());

    // // Create a shared UART channel for the console and for kernel debug.
    // let uart_mux =
    //     components::console::UartMuxComponent::new(channel, 115200, dynamic_deferred_caller)
    //         .finalize(());

    // // Create the debugger object that handles calls to `debug!()`.
    // components::debug_writer::DebugWriterComponent::new(uart_mux).finalize(());

    //--------------------------------------------------------------------------
    // CDC
    //--------------------------------------------------------------------------

    // Setup the CDC-ACM over USB driver that we will use for UART.
    // We use the Arduino Vendor ID and Product ID since the device is the same.

    // Create the strings we include in the USB descriptor. We use the hardcoded
    // DEVICEADDR register on the nRF52 to set the serial number.
    let serial_number_buf = static_init!([u8; 17], [0; 17]);
    let serial_number_string: &'static str =
        nrf52::ficr::FICR_INSTANCE.address_str(serial_number_buf);
    let strings = static_init!(
        [&str; 3],
        [
            "Arduino",              // Manufacturer
            "Nano 33 BLE - TockOS", // Product
            serial_number_string,   // Serial number
        ]
    );

    let cdc = components::cdc::CdcAcmComponent::new(
        &nrf52840_peripherals.usbd,
        capsules::usb::cdc::MAX_CTRL_PACKET_SIZE_NRF52840,
        0x2341,
        0x005a,
        strings,
        mux_alarm,
        dynamic_deferred_caller,
        None,
    )
    .finalize(components::usb_cdc_acm_component_helper!(
        nrf52::usbd::Usbd,
        nrf52::rtc::Rtc
    ));

    //--------------------------------------------------------------------------
    // BOOTLOADER
    //--------------------------------------------------------------------------

    let recv_auto_virtual_alarm = static_init!(
        VirtualMuxAlarm<'static, nrf52::rtc::Rtc>,
        VirtualMuxAlarm::new(mux_alarm)
    );

    let recv_auto_cdc = static_init!(
        bootloader::uart_receive_multiple_timeout::UartReceiveMultipleTimeout<
            'static,
            VirtualMuxAlarm<'static, nrf52::rtc::Rtc>,
        >,
        bootloader::uart_receive_multiple_timeout::UartReceiveMultipleTimeout::new(
            cdc,
            recv_auto_virtual_alarm,
            &mut bootloader::uart_receive_multiple_timeout::BUF,
        )
    );
    recv_auto_virtual_alarm.set_alarm_client(recv_auto_cdc);

    let nrfpagebuffer = static_init!(nrf52::nvmc::NrfPage, nrf52::nvmc::NrfPage::default());

    let flash_adapter = static_init!(
        bootloader::flash_large_to_small::FlashLargeToSmall<'static, nrf52::nvmc::Nvmc>,
        bootloader::flash_large_to_small::FlashLargeToSmall::new(
            &base_peripherals.nvmc,
            nrfpagebuffer,
        )
    );
    hil::flash::HasClient::set_client(&base_peripherals.nvmc, flash_adapter);

    let pagebuffer = static_init!(
        bootloader::flash_large_to_small::FiveTwelvePage,
        bootloader::flash_large_to_small::FiveTwelvePage::default()
    );

    let bootloader = static_init!(
        bootloader::bootloader::Bootloader<
            'static,
            bootloader::uart_receive_multiple_timeout::UartReceiveMultipleTimeout<
                'static,
                VirtualMuxAlarm<'static, nrf52::rtc::Rtc>,
            >,
            bootloader::flash_large_to_small::FlashLargeToSmall<'static, nrf52::nvmc::Nvmc>,
        >,
        bootloader::bootloader::Bootloader::new(
            recv_auto_cdc,
            flash_adapter,
            &bootloader_exit,
            pagebuffer,
            &mut bootloader::bootloader::BUF
        )
    );
    hil::uart::Transmit::set_transmit_client(cdc, bootloader);
    hil::uart::Receive::set_receive_client(cdc, recv_auto_cdc);
    hil::uart::Receive::set_receive_client(recv_auto_cdc, bootloader);
    hil::flash::HasClient::set_client(flash_adapter, bootloader);

    //--------------------------------------------------------------------------
    // ALTERNATIVE BOOTLOADER STACK
    //
    // This code is a starting point for using the bootloader on top of a normal
    // UART rather than the USB stack.
    //--------------------------------------------------------------------------

    // // Setup receive with timeout.
    // let recv_auto_virtual_alarm = static_init!(
    //     VirtualMuxAlarm<'static, nrf5x::rtc::Rtc>,
    //     VirtualMuxAlarm::new(mux_alarm)
    // );

    // let recv_auto_uart = static_init!(
    //     bootloader::uart_receive_timeout::UartReceiveTimeout<
    //         'static,
    //         VirtualMuxAlarm<'static, nrf5x::rtc::Rtc>,
    //     >,
    //     bootloader::uart_receive_timeout::UartReceiveTimeout::new(
    //         &nrf52::uart::UARTE0,
    //         recv_auto_virtual_alarm,
    //         &nrf5x::gpio::PORT[UART_RXD]
    //     )
    // );
    // recv_auto_virtual_alarm.set_client(recv_auto_uart);
    // nrf5x::gpio::PORT[UART_RXD].set_client(recv_auto_uart);
    // recv_auto_uart.initialize();

    // // Setup the UART pins
    // nrf52::uart::UARTE0.initialize(
    //     nrf5x::pinmux::Pinmux::new(UART_TXD as u32),
    //     nrf5x::pinmux::Pinmux::new(UART_RXD as u32),
    //     nrf5x::pinmux::Pinmux::new(UART_CTS as u32),
    //     nrf5x::pinmux::Pinmux::new(UART_RTS as u32),
    // );

    // let nrfpagebuffer = static_init!(nrf52::nvmc::NrfPage, nrf52::nvmc::NrfPage::default());

    // let flash_adapter = static_init!(
    //     bootloader::flash_large_to_small::FlashLargeToSmall<'static, nrf52::nvmc::Nvmc>,
    //     bootloader::flash_large_to_small::FlashLargeToSmall::new(
    //         &base_peripherals.nvmc,
    //         nrfpagebuffer,
    //     )
    // );
    // hil::flash::HasClient::set_client(&base_peripherals.nvmc, flash_adapter);

    // let pagebuffer = static_init!(
    //     bootloader::flash_large_to_small::FiveTwelvePage,
    //     bootloader::flash_large_to_small::FiveTwelvePage::default()
    // );

    // let bootloader = static_init!(
    //     bootloader::bootloader::Bootloader<
    //         'static,
    //         bootloader::uart_receive_timeout::UartReceiveTimeout<
    //         'static,
    //         VirtualMuxAlarm<'static, nrf5x::rtc::Rtc>,
    //     >,
    //         bootloader::flash_large_to_small::FlashLargeToSmall<'static, nrf52::nvmc::Nvmc>,
    //     >,
    //     bootloader::bootloader::Bootloader::new(
    //         nrf52::uart::UARTE0,
    //         flash_adapter,
    //         &bootloader_exit,
    //         pagebuffer,
    //         &mut bootloader::bootloader::BUF
    //     )
    // );
    // hil::uart::Transmit::set_transmit_client(&nrf52::uart::UARTE0, bootloader);
    // hil::uart::Receive::set_receive_client(&nrf52::uart::UARTE0, bootloader);
    // hil::flash::HasClient::set_client(flash_adapter, bootloader);

    //--------------------------------------------------------------------------
    // FINAL SETUP AND BOARD BOOT
    //--------------------------------------------------------------------------

    // Start all of the clocks. Low power operation will require a better
    // approach than this.
    nrf52_components::NrfClockComponent::new(&base_peripherals.clock).finalize(());

    let platform = Platform { bootloader };

    let chip = static_init!(
        nrf52840::chip::NRF52<Nrf52840DefaultPeripherals>,
        nrf52840::chip::NRF52::new(nrf52840_peripherals)
    );
    CHIP = Some(chip);

    // Need to disable the MPU because the bossa bootloader seems to set it up.
    chip.mpu().clear_mpu();

    // Configure the USB stack to enable a serial port over CDC-ACM.
    cdc.enable();
    cdc.attach();

    // Actually run the bootloader.
    platform.bootloader.start();

    //--------------------------------------------------------------------------
    // MAIN LOOP
    //--------------------------------------------------------------------------

    let scheduler = components::sched::round_robin::RoundRobinComponent::new(&PROCESSES)
        .finalize(components::rr_component_helper!(NUM_PROCS));
    board_kernel.kernel_loop::<_, _, _, NUM_PROCS>(
        &platform,
        chip,
        None,
        scheduler,
        &main_loop_capability,
    );
}

#[cfg(not(test))]
#[no_mangle]
#[panic_handler]
pub unsafe extern "C" fn panic_fmt(_pi: &PanicInfo) -> ! {
    loop {}
}
