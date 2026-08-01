#![no_std]
#![no_main]

use core::fmt::Write;
use heapless::String;

use panic_probe as _;
use defmt_rtt as _;

use embassy_executor::Spawner;
use embassy_stm32::{
    bind_interrupts, dma, i2c, peripherals,
    gpio::{Level, Output, Speed},
    i2c::I2c,
    rcc::{
        AHBPrescaler, APBPrescaler, Hse, HseMode, Pll, PllMul, PllPDiv, PllPreDiv, PllQDiv, PllRDiv,
        PllSource, Sysclk,
    },
    spi::{Config as SpiConfig, Spi},
    time::Hertz,
};
use embassy_time::{Delay, Timer};

use embedded_hal_bus::spi::ExclusiveDevice;
use mipidsi::{
    interface::SpiInterface,
    models::ILI9488Rgb666,
    options::{ColorOrder, Orientation},
    Builder,
};

use embedded_graphics::{
    geometry::{Point, Size},
    pixelcolor::Rgb666,
    prelude::*,
    primitives::{CornerRadii, PrimitiveStyle, Rectangle, RoundedRectangle},
};
use embedded_graphics_framebuf::FrameBuf;
use embedded_graphics_profont::{Anchor, Text, WithAnchor};

use ds323x::{DateTimeAccess, Ds323x, Timelike};

mod fonts;
use fonts::D_DIN41X44 as D_DIN;

// -----------------------------------------------------------------------------
// Global Static Buffers (placed in SRAM1 for DMA access & Rust 2024 compliance)
// -----------------------------------------------------------------------------

/// Display interface SPI transmit buffer
#[unsafe(link_section = ".sram1")]
static mut DI_BUFFER: [u8; 16384] = [0u8; 16384];

/// Time section partial framebuffer (280x60 pixels)
const TIME_FB_W: usize = 280;
const TIME_FB_H: usize = 60;

#[unsafe(link_section = ".sram1")]
static mut TIME_FB_DATA: [Rgb666; TIME_FB_W * TIME_FB_H] = [Rgb666::BLACK; TIME_FB_W * TIME_FB_H];

// Interrupt bindings for Embassy hardware peripherals
bind_interrupts!(struct Irqs {
    DMA1_STREAM0 => dma::InterruptHandler<peripherals::DMA1_CH0>;
    DMA1_STREAM6 => dma::InterruptHandler<peripherals::DMA1_CH6>;
    DMA2_STREAM3 => dma::InterruptHandler<peripherals::DMA2_CH3>;
    I2C1_EV => i2c::EventInterruptHandler<peripherals::I2C1>;
    I2C1_ER => i2c::ErrorInterruptHandler<peripherals::I2C1>;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // -------------------------------------------------------------------------
    // 1. System Clock Configuration (216 MHz Sysclk, 54 MHz APB1, 108 MHz APB2)
    // -------------------------------------------------------------------------
    let mut config = embassy_stm32::Config::default();
    
    // Configure 8 MHz HSE (Bypass mode for Nucleo boards)
    config.rcc.hse = Some(Hse {
        freq: Hertz(8_000_000),
        mode: HseMode::Bypass,
    });
    config.rcc.pll_src = PllSource::HSE;

    // PLL configuration: (8 MHz / 4) * 216 / 2 = 216 MHz
    config.rcc.pll = Some(Pll {
        prediv: PllPreDiv::DIV4,
        mul: PllMul::MUL216,
        divp: Some(PllPDiv::DIV2),
        divq: Some(PllQDiv::DIV9),
        divr: Some(PllRDiv::DIV2),
    });

    // Bus prescalers to keep peripherals within clock limits
    config.rcc.ahb_pre = AHBPrescaler::DIV1;   // HCLK = 216 MHz
    config.rcc.apb1_pre = APBPrescaler::DIV4;  // PCLK1 = 54 MHz (Max 54 MHz)
    config.rcc.apb2_pre = APBPrescaler::DIV2;  // PCLK2 = 108 MHz (Max 108 MHz)
    config.rcc.sys = Sysclk::PLL1_P;

    let p = embassy_stm32::init(config);

    // -------------------------------------------------------------------------
    // 2. I2C1 Peripheral & DS3231 RTC Initialization
    // -------------------------------------------------------------------------
    let i2c_dev = I2c::new(
        p.I2C1,
        p.PB8, // SCL
        p.PB9, // SDA
        p.DMA1_CH6,
        p.DMA1_CH0,
        Irqs,
        Default::default(),
    );

    let mut rtc = Ds323x::new_ds3231(i2c_dev);
    rtc.enable().unwrap();

    // -------------------------------------------------------------------------
    // 3. Display Control GPIOs & SPI Interface Initialization
    // -------------------------------------------------------------------------
    let cs = Output::new(p.PC7, Level::High, Speed::VeryHigh);
    let dc = Output::new(p.PC6, Level::Low, Speed::VeryHigh);
    let rst = Output::new(p.PA4, Level::High, Speed::VeryHigh);

    let mut spi_config = SpiConfig::default();
    spi_config.frequency = Hertz(54_000_000); // 40 MHz SPI clock

    let spi = Spi::new_txonly(
        p.SPI1,
        p.PA5,      // SCK
        p.PA7,      // MOSI
        p.DMA2_CH3, // DMA TX Channel
        Irqs,
        spi_config,
    );

    let spi_device = ExclusiveDevice::new_no_delay(spi, cs).unwrap();
    let di_buf = unsafe { &mut *core::ptr::addr_of_mut!(DI_BUFFER) };
    let di = SpiInterface::new(spi_device, dc, di_buf);

    // Initialize ILI9488 LCD Driver
    let mut display = Builder::new(ILI9488Rgb666, di)
        .reset_pin(rst)
        .color_order(ColorOrder::Bgr)
        .orientation(Orientation::default().flip_horizontal())
        .init(&mut Delay)
        .unwrap();

    display.clear(Rgb666::BLACK).unwrap();
    Timer::after_millis(100).await;

    // -------------------------------------------------------------------------
    // 4. UI Layout & State Variables
    // -------------------------------------------------------------------------
    let time_screen_pos = Point::new(20, 40);
    let time_area = Rectangle::new(time_screen_pos, Size::new(TIME_FB_W as u32, TIME_FB_H as u32));
    let temp_pos = Point::new(150, 300);

    let mut previous_temp: f32 = -999.0;

    // -------------------------------------------------------------------------
    // 5. Main Application Loop
    // -------------------------------------------------------------------------
    loop {
        // --- A. Render Time (RAM Framebuffer to avoid screen flicker) ---
        if let Ok(dt) = rtc.datetime() {
            let mut time_str: String<32> = String::new();
            write!(time_str, "{:02}:{:02}:{:02}", dt.hour(), dt.minute(), dt.second()).ok();

            // 1. Instantiating FrameBuffer in RAM
            let fb_data = unsafe { &mut *core::ptr::addr_of_mut!(TIME_FB_DATA) };
            let mut time_fb = FrameBuf::new(fb_data, TIME_FB_W, TIME_FB_H);

            // 2. Clear buffer and draw text in RAM (< 1ms)
            time_fb.clear(Rgb666::BLACK).ok();
            Text::new(&time_str, Point::new(0, 50), &D_DIN, Rgb666::WHITE)
                .with_anchor(Anchor::BottomLeft)
                .with_tracking(10)
                .draw(&mut time_fb)
                .ok();

            // 3. Flush pixel data to display via DMA
            let fb_slice = unsafe { &*core::ptr::addr_of!(TIME_FB_DATA) };
            display.fill_contiguous(&time_area, fb_slice.iter().copied()).ok();
        }

        // --- B. Render Temperature (Update only on value change) ---
        if let Ok(temperature) = rtc.temperature() {
            if (previous_temp - temperature).abs() > 0.01 {
                let mut temp_str: String<32> = String::new();
                write!(temp_str, "{:.2} deg C", temperature).ok();

                let rect_size = Size::new(
                    D_DIN.measure_str(&temp_str, 0) + 10,
                    (D_DIN.max_height as u32) + 10,
                );

                // Draw background box
                RoundedRectangle::new(
                    Rectangle::with_center(temp_pos, rect_size),
                    CornerRadii::new(Size::new(15, 15)),
                )
                .into_styled(PrimitiveStyle::with_fill(Rgb666::BLUE))
                .draw(&mut display)
                .ok();

                // Draw temperature text
                Text::new(&temp_str, temp_pos, &D_DIN, Rgb666::WHITE)
                    .with_anchor(Anchor::MiddleCenter)
                    .draw(&mut display)
                    .ok();

                previous_temp = temperature;
            }
        }

        Timer::after_millis(200).await;
    }
}