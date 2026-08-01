#![no_std]
#![no_main]

use core::fmt::Write;
use heapless::String;

use panic_probe as _;
use defmt_rtt as _;
use defmt::{warn, error};

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

#[unsafe(link_section = ".sram1")]
static mut DI_BUFFER: [u8; 16384] = [0u8; 16384];

const TIME_FB_W: usize = 280;
const TIME_FB_H: usize = 60;

#[unsafe(link_section = ".sram1")]
static mut TIME_FB_DATA: [Rgb666; TIME_FB_W * TIME_FB_H] = [Rgb666::BLACK; TIME_FB_W * TIME_FB_H];

bind_interrupts!(struct Irqs {
    DMA1_STREAM0 => dma::InterruptHandler<peripherals::DMA1_CH0>;
    DMA1_STREAM6 => dma::InterruptHandler<peripherals::DMA1_CH6>;
    DMA2_STREAM3 => dma::InterruptHandler<peripherals::DMA2_CH3>;
    I2C1_EV => i2c::EventInterruptHandler<peripherals::I2C1>;
    I2C1_ER => i2c::ErrorInterruptHandler<peripherals::I2C1>;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut config = embassy_stm32::Config::default();

    config.rcc.hse = Some(Hse {
        freq: Hertz(8_000_000),
        mode: HseMode::Bypass,
    });
    config.rcc.pll_src = PllSource::HSE;

    config.rcc.pll = Some(Pll {
        prediv: PllPreDiv::DIV4,
        mul: PllMul::MUL216,
        divp: Some(PllPDiv::DIV2),
        divq: Some(PllQDiv::DIV9),
        divr: Some(PllRDiv::DIV2),
    });

    config.rcc.ahb_pre = AHBPrescaler::DIV1;
    config.rcc.apb1_pre = APBPrescaler::DIV4;
    config.rcc.apb2_pre = APBPrescaler::DIV2;
    config.rcc.sys = Sysclk::PLL1_P;

    let p = embassy_stm32::init(config);

    let i2c_dev = I2c::new(
        p.I2C1,
        p.PB8,
        p.PB9,
        p.DMA1_CH6,
        p.DMA1_CH0,
        Irqs,
        Default::default(),
    );

    let mut rtc = Ds323x::new_ds3231(i2c_dev);

    if let Err(e) = rtc.enable() {
        error!("Failed to enable DS3231 RTC: {:?}", defmt::Debug2Format(&e));
    }

    let cs = Output::new(p.PC7, Level::High, Speed::VeryHigh);
    let dc = Output::new(p.PC6, Level::Low, Speed::VeryHigh);
    let rst = Output::new(p.PA4, Level::High, Speed::VeryHigh);

    let mut spi_config = SpiConfig::default();
    spi_config.frequency = Hertz(20_000_000);

    let spi = Spi::new_txonly(
        p.SPI1,
        p.PA5,
        p.PA7,
        p.DMA2_CH3,
        Irqs,
        spi_config,
    );

    let spi_device = match ExclusiveDevice::new_no_delay(spi, cs) {
        Ok(dev) => dev,
        Err(_) => {
            error!("Failed to create SPI device wrapper");
            loop {
                Timer::after_millis(1000).await;
            }
        }
    };

    let di_buf = unsafe { &mut *core::ptr::addr_of_mut!(DI_BUFFER) };
    let di = SpiInterface::new(spi_device, dc, di_buf);

    let mut display = match Builder::new(ILI9488Rgb666, di)
        .reset_pin(rst)
        .color_order(ColorOrder::Bgr)
        .orientation(Orientation::default().flip_horizontal())
        .init(&mut Delay)
    {
        Ok(d) => d,
        Err(_) => {
            error!("Display init failed");
            loop {
                Timer::after_millis(1000).await;
            }
        }
    };

    if display.clear(Rgb666::BLACK).is_err() {
        warn!("Initial screen clear failed");
    }
    Timer::after_millis(100).await;

    let time_screen_pos = Point::new(20, 40);
    let time_area = Rectangle::new(time_screen_pos, Size::new(TIME_FB_W as u32, TIME_FB_H as u32));
    let temp_pos = Point::new(150, 300);

    let temp_box_size = Size::new(180, (D_DIN.max_height as u32) + 10);

    let mut previous_temp: f32 = f32::NAN;
    let mut previous_second: u32 = u32::MAX;

    loop {
        match rtc.datetime() {
            Ok(dt) => {
                let second = dt.second();
                if second != previous_second {
                    previous_second = second;

                    let mut time_str: String<32> = String::new();
                    if write!(time_str, "{:02}:{:02}:{:02}", dt.hour(), dt.minute(), second).is_err() {
                        warn!("Time string formatting overflowed buffer");
                    }

                    let fb_data = unsafe { &mut *core::ptr::addr_of_mut!(TIME_FB_DATA) };
                    let mut time_fb = FrameBuf::new(fb_data, TIME_FB_W, TIME_FB_H);

                    if time_fb.clear(Rgb666::BLACK).is_err() {
                        warn!("Time framebuffer clear failed");
                    }
                    if Text::new(&time_str, Point::new(0, 50), &D_DIN, Rgb666::WHITE)
                        .with_anchor(Anchor::BottomLeft)
                        .with_tracking(10)
                        .draw(&mut time_fb)
                        .is_err()
                    {
                        warn!("Time text draw failed");
                    }

                    let fb_slice = unsafe { &*core::ptr::addr_of!(TIME_FB_DATA) };
                    if display
                        .fill_contiguous(&time_area, fb_slice.iter().copied())
                        .is_err()
                    {
                        warn!("Time area DMA flush failed");
                    }
                }
            }
            Err(_) => warn!("Failed to read datetime from RTC"),
        }

        match rtc.temperature() {
            Ok(temperature) => {
                if (previous_temp - temperature).abs() > 0.01 || previous_temp.is_nan() {
                    let mut temp_str: String<32> = String::new();
                    if write!(temp_str, "{:.2} deg C", temperature).is_err() {
                        warn!("Temperature string formatting overflowed buffer");
                    }

                    if RoundedRectangle::new(
                        Rectangle::with_center(temp_pos, temp_box_size),
                        CornerRadii::new(Size::new(15, 15)),
                    )
                    .into_styled(PrimitiveStyle::with_fill(Rgb666::BLUE))
                    .draw(&mut display)
                    .is_err()
                    {
                        warn!("Temperature background draw failed");
                    }

                    if Text::new(&temp_str, temp_pos, &D_DIN, Rgb666::WHITE)
                        .with_anchor(Anchor::MiddleCenter)
                        .draw(&mut display)
                        .is_err()
                    {
                        warn!("Temperature text draw failed");
                    }

                    previous_temp = temperature;
                }
            }
            Err(_) => warn!("Failed to read temperature from RTC"),
        }

        Timer::after_millis(200).await;
    }
}