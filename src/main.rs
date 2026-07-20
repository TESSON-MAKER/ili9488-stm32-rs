#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::spi::{Config, Spi};
use embassy_stm32::{bind_interrupts, dma, peripherals};
use embassy_time::{Delay, Timer};
use embassy_stm32::time::Hertz;
use mipidsi::interface::SpiInterface;
use mipidsi::{Builder, models::ILI9488Rgb666, options::ColorOrder};
use embedded_hal_bus::spi::ExclusiveDevice;
use panic_probe as _;
use defmt_rtt as _;

use embedded_graphics::{
    pixelcolor::Rgb666,
    prelude::*,
    primitives::{Circle, PrimitiveStyle},
};

// Configuration des interruptions pour le DMA2 (SPI1)
bind_interrupts!(struct Irqs {
    DMA2_STREAM3 => dma::InterruptHandler<peripherals::DMA2_CH3>;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // Initialisation du STM32 avec la configuration par défaut
    let p = embassy_stm32::init(Default::default());

    // 1. Configuration des broches de contrôle (GPIO)
    // CS et RST doivent être à l'état HAUT (High) par défaut (inactifs)
    let cs = Output::new(p.PC7, Level::High, Speed::VeryHigh); 
    let dc = Output::new(p.PC6, Level::Low, Speed::VeryHigh);
    let rst = Output::new(p.PA4, Level::High, Speed::VeryHigh); 

    // 2. Configuration du bus SPI Hardware
    let mut spi_config = Config::default();
    //spi_config.mode = embassy_stm32::spi::MODE_0; // Mode SPI 0 classique pour l'ILI9488
    spi_config.frequency = Hertz(16_000_000);       // 16 MHz : stable et sécurisé pour les tests

    let spi = Spi::new_txonly(
        p.SPI1, 
        p.PA5,       // SCK
        p.PA7,       // MOSI
        p.DMA2_CH3,  // Canal DMA
        Irqs,
        spi_config, 
    );

    // Encapsulation du SPI avec le CS géré automatiquement par embedded-hal-bus
    let spi_device = ExclusiveDevice::new(spi, cs, Delay).unwrap();
    
    // Buffer requis par le driver pour stocker les commandes/données d'affichage
    let mut di_buffer = [0u8; 2048];
    let di = SpiInterface::new(spi_device, dc, &mut di_buffer);
    
    // 3. Initialisation de l'écran avec mipidsi
    let mut display = Builder::new(ILI9488Rgb666, di)
        .reset_pin(rst)
        .color_order(ColorOrder::Bgr)
        .init(&mut Delay)
        .unwrap();

    // 4. Test d'affichage direct (Avant d'entrer dans la boucle infinie)
    // On remplit d'abord tout l'écran en BLANC
    display.clear(Rgb666::WHITE).unwrap();

    // 5. Boucle principale (Maintient le CPU actif sans bloquer le premier rendu)
    loop {
        Timer::after_millis(10).await;


        Circle::new(Point::new(15, 15), 60)
            .translate(Point::new(20, 10))
            .into_styled(PrimitiveStyle::with_fill(Rgb666::MAGENTA))
            .draw(&mut display).ok();
    }
}