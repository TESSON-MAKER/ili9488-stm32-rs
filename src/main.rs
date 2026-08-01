#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::i2c::I2c;
use embassy_stm32::spi::{Config, Spi};
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, dma, i2c, peripherals};
use embassy_time::{Delay, Timer};

use core::fmt::Write;
use heapless::String;

use embedded_hal_bus::spi::ExclusiveDevice;
use mipidsi::interface::SpiInterface;
use mipidsi::{
    models::ILI9488Rgb666,
    options::{ColorOrder, Orientation},
    Builder,
};
use static_cell::StaticCell;

use panic_probe as _;
use defmt_rtt as _;

use embedded_graphics::{
    pixelcolor::Rgb666,
    prelude::*,
    primitives::{Circle, PrimitiveStyle},
};

use embedded_graphics_profont::{Anchor, Text, WithAnchor};

use ds323x::{Ds323x, Timelike, DateTimeAccess};

mod fonts;
use fonts::D_DIN41X44 as D_DIN;

// Buffer d'interface statique de 16 Ko pour maximiser les paquets DMA
static DI_BUFFER: StaticCell<[u8; 16384]> = StaticCell::new();

bind_interrupts!(struct Irqs {
    DMA1_STREAM0 => dma::InterruptHandler<peripherals::DMA1_CH0>; // For I2C1 TX
    DMA1_STREAM6 => dma::InterruptHandler<peripherals::DMA1_CH6>; // For I2C1 RX
    DMA2_STREAM3 => dma::InterruptHandler<peripherals::DMA2_CH3>; // For SPI1 TX
    I2C1_EV => i2c::EventInterruptHandler<peripherals::I2C1>;
    I2C1_ER => i2c::ErrorInterruptHandler<peripherals::I2C1>;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // 1. Initialisation par défaut du STM32 (horloge d'origine)
    let p = embassy_stm32::init(Default::default());

    // 2. Configuration des broches GPIO pour l'I2C
    let dev = I2c::new(
        p.I2C1,
        p.PB8, 
        p.PB9, 
        p.DMA1_CH6, 
        p.DMA1_CH0,
        Irqs,
        Default::default(),
    );

    let mut rtc = Ds323x::new_ds3231(dev);
    rtc.enable().unwrap(); // Activation du composant RTC

    let cs = Output::new(p.PC7, Level::High, Speed::VeryHigh); 
    let dc = Output::new(p.PC6, Level::Low, Speed::VeryHigh);
    let rst = Output::new(p.PA4, Level::High, Speed::VeryHigh); 

    // 3. Configuration du bus SPI à la fréquence d'origine (16 MHz)
    let mut spi_config = Config::default();
    spi_config.frequency = Hertz(16_000_000); 

    let spi = Spi::new_txonly(
        p.SPI1, 
        p.PA5,       // SCK
        p.PA7,       // MOSI
        p.DMA2_CH3,  // Canal DMA
        Irqs,
        spi_config, 
    );

    let spi_device = ExclusiveDevice::new_no_delay(spi, cs).unwrap();
    
    // 4. Driver Écran avec buffer statique de 16 Ko et modèle ILI9488Rgb666
    let di_buffer = DI_BUFFER.init([0u8; 16384]);
    let di = SpiInterface::new(spi_device, dc, di_buffer);
    
    let mut display = Builder::new(ILI9488Rgb666, di)
        .reset_pin(rst)
        .color_order(ColorOrder::Bgr)
        .orientation(Orientation::default().flip_horizontal())
        .init(&mut Delay)
        .unwrap();
    
    display.clear(Rgb666::BLACK).unwrap();
    Timer::after_millis(100).await;

    // Positions d'affichage
    let time_pos = Point::new(20, 80);
    let temp_pos = Point::new(20, 150);
        
    loop {
        // --- 1. Lecture de la date et de l'heure complètes ---
        let dt = rtc.datetime().unwrap();

        let mut time_buf: String<32> = String::new();
        write!(time_buf, "{:02}:{:02}:{:02}", dt.hour(), dt.minute(), dt.second()).ok();

        Text::new(&time_buf, time_pos, &D_DIN, Rgb666::WHITE)
            .with_anchor(Anchor::BottomLeft)
            .with_background_color(Rgb666::BLACK)
            .with_tracking(10)
            .draw(&mut display)
            .ok();

        // --- 2. Affichage de la Température ---
        let temperature = rtc.temperature().unwrap();
        let mut temp_buf: String<32> = String::new();
        write!(temp_buf, "{:.2} deg C", temperature).ok();

        Text::new(&temp_buf, temp_pos, &D_DIN, Rgb666::WHITE)
            .with_anchor(Anchor::BottomLeft)
            .with_background_color(Rgb666::RED)
            .draw(&mut display)
            .ok();

        // Indicateur visuel
        Circle::with_center(temp_pos, 5)
            .into_styled(PrimitiveStyle::with_fill(Rgb666::CSS_ORANGE))
            .draw(&mut display)
            .ok();

        Timer::after_millis(500).await;
    }
}