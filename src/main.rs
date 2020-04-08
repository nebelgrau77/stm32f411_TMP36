//! Reading temperature from a TMP36 sensor every second    
//! 
//! Averaging of N-X ADC samples for accurate conversion: 
//! - voltage is sampled 12 times, then sorted, the two biggest and two smallest values are dropped
//! - final sample is obtained by averaging the remaining 8 values 
//! 
//! Stable display value:
//! - display a moving average of last 8 samples
//! 
//! Button press switches between Celsius and Fahrenheit degrees


#![no_std]
#![no_main]

extern crate cortex_m;
extern crate cortex_m_rt as rt;
extern crate panic_halt;
extern crate stm32f4xx_hal as hal;

use cortex_m_rt::entry;
use cortex_m::interrupt::{Mutex, free};

use core::ops::DerefMut;
use core::cell::{Cell, RefCell};

use stm32f4::stm32f411::interrupt;

use crate::hal::{
    i2c::I2c, 
    prelude::*, 
    gpio::{gpioa::{PA0, PA4}, Analog, Input, PullUp},
    stm32,
    delay::Delay,
    adc::{Adc, config::{AdcConfig, SampleTime, Clock, Resolution}},
    timer::{Timer, Event},
    time::Hertz,
    stm32::Interrupt,
    };

use ssd1306::{
    prelude::*, 
    Builder as SSD1306Builder
    };

use embedded_graphics::{
    fonts::{Font12x16, Text},
    pixelcolor::BinaryColor,
    prelude::*,
    style::TextStyleBuilder,
    };

use core::fmt;
use arrayvec::ArrayString;

// globally accessible values
static TEMP_C: Mutex<Cell<i16>> = Mutex::new(Cell::new(0i16));
static TEMP_F: Mutex<Cell<i16>> = Mutex::new(Cell::new(0i16));

static BUF: Mutex<Cell<[u16;8]>> = Mutex::new(Cell::new([0u16;8]));

static FLAG: Mutex<Cell<bool>> = Mutex::new(Cell::new(true));

static STATE1: Mutex<Cell<bool>> = Mutex::new(Cell::new(false));
static STATE2: Mutex<Cell<bool>> = Mutex::new(Cell::new(false)); //adding the second state

// interrupt and peripherals for ADC
static TIMER_TIM3: Mutex<RefCell<Option<Timer<stm32::TIM3>>>> = Mutex::new(RefCell::new(None));
static TIMER_TIM2: Mutex<RefCell<Option<Timer<stm32::TIM2>>>> = Mutex::new(RefCell::new(None));

static BUTTON: Mutex<RefCell<Option<PA0<Input<PullUp>>>>> = Mutex::new(RefCell::new(None));

static GADC: Mutex<RefCell<Option<Adc<stm32::ADC1>>>> = Mutex::new(RefCell::new(None));
static ANALOG: Mutex<RefCell<Option<PA4<Analog>>>> = Mutex::new(RefCell::new(None));

const FACTOR: f32 = 3300.0/4096.0; //3300 mV / 4096 values for 12-bit ADC

const BOOT_DELAY_MS: u16 = 200; //delay for the I2C to start correctly after power up

#[entry]
fn main() -> ! {
    if let (Some(dp), Some(cp)) = (
        stm32::Peripherals::take(),
        cortex_m::peripheral::Peripherals::take(),
) {
        // Set up the system clock. Speed is not important in this case
        
        let rcc = dp.RCC.constrain();
        let clocks = rcc.cfgr.use_hse(25.mhz()).sysclk(25.mhz()).freeze();
        
        let mut delay = Delay::new(cp.SYST, clocks);
        
        //delay necessary for the I2C to initiate correctly and start on boot without having to reset the board
        delay.delay_ms(BOOT_DELAY_MS);

        //set up ADC
        let gpioa = dp.GPIOA.split();
        let adcconfig = AdcConfig::default().clock(Clock::Pclk2_div_8).resolution(Resolution::Twelve);
        let adc = Adc::adc1(dp.ADC1, true, adcconfig);
                
        let pa4 = gpioa.pa4.into_analog();

        // set up on-board button on PA0
        
        let mut board_btn = gpioa.pa0.into_pull_up_input();

        // move the PA4 pin and the ADC into the 'global storage'
        free(|cs| {
            *GADC.borrow(cs).borrow_mut() = Some(adc);        
            *ANALOG.borrow(cs).borrow_mut() = Some(pa4);            
        });

        // Set up I2C - SCL is PB8 and SDA is PB9; they are set to Alternate Function 4
        let gpiob = dp.GPIOB.split();
        let scl = gpiob.pb8.into_alternate_af4().set_open_drain();
        let sda = gpiob.pb9.into_alternate_af4().set_open_drain();
        let i2c = I2c::i2c1(dp.I2C1, (scl, sda), 400.khz(), clocks);

        // Set up the display
        let mut disp: GraphicsMode<_> = SSD1306Builder::new().size(DisplaySize::Display128x32).connect_i2c(i2c).into();
        disp.init().unwrap();

        // set up timer and interrupts
        let mut adctimer = Timer::tim3(dp.TIM3, Hertz(1), clocks); //interrupt will fire every second
        adctimer.listen(Event::TimeOut);

        let mut btntimer = Timer::tim2(dp.TIM2, Hertz(20), clocks);
        btntimer.listen(Event::TimeOut);

                
        free(|cs| {
            TIMER_TIM3.borrow(cs).replace(Some(adctimer));
            TIMER_TIM2.borrow(cs).replace(Some(btntimer));
            BUTTON.borrow(cs).replace(Some(board_btn));
            });

        let mut nvic = cp.NVIC;
            unsafe {            
                nvic.set_priority(Interrupt::TIM3, 1);
                nvic.set_priority(Interrupt::TIM2, 2);
                cortex_m::peripheral::NVIC::unmask(Interrupt::TIM3);
                cortex_m::peripheral::NVIC::unmask(Interrupt::TIM2);
            }
                        
            cortex_m::peripheral::NVIC::unpend(Interrupt::TIM3);
            cortex_m::peripheral::NVIC::unpend(Interrupt::TIM2);


        //set up text style for the display
        let text_style = TextStyleBuilder::new(Font12x16).text_color(BinaryColor::On).build();

        loop {
                        
            let mut buf_temp = ArrayString::<[u8; 8]>::new(); //buffer for the temperature reading
            //let mut buf_temp_c = ArrayString::<[u8; 8]>::new(); //buffer for the temperature reading
            //let mut buf_temp_f = ArrayString::<[u8; 8]>::new(); //buffer for the temperature reading
        
            //clean up the display    
            for x in 0..96 {
                for y in 0..16 {
                    disp.set_pixel(x,y,0);
                }
            }

            let flag = free(|cs| FLAG.borrow(cs).get());

            let celsius = free(|cs| TEMP_C.borrow(cs).get()); //get the current temperature in Celsius
            let fahrenheit = free(|cs| TEMP_F.borrow(cs).get()); //get the current temperature in Fahrenheit
            
            if flag {
                
                formatter(&mut buf_temp, celsius, 67 as char); // 67 is "C" in ASCII
                

            } else {

                formatter(&mut buf_temp, fahrenheit, 70 as char); // 70 is "F" in ASCII
                
            }

            Text::new(buf_temp.as_str(), Point::new(0, 0)).into_styled(text_style).draw(&mut disp);

            disp.flush().unwrap();
            
            delay.delay_ms(20_u16); //update the display every 20 ms
            
            }

        }

    loop {}
}

#[interrupt]

// read from ADC on pin PA4 and update the global values every second

fn TIM3() {
        
    free(|cs| {
        stm32::NVIC::unpend(Interrupt::TIM3);
        if let (Some(ref mut tim3), Some(ref mut adc), Some(ref mut analog)) = (
        TIMER_TIM3.borrow(cs).borrow_mut().deref_mut(),
        GADC.borrow(cs).borrow_mut().deref_mut(),
        
        ANALOG.borrow(cs).borrow_mut().deref_mut())
        
        {
            tim3.clear_interrupt(Event::TimeOut);
                        
            //sample the temperature from the TMP36 sensor 12 times
            let mut adc_buf: [u16;12] = [0u16;12]; 

            for n in 0..12 {
                let sample = adc.convert(analog, SampleTime::Cycles_144);
                adc_buf[n] = sample;
            }

            //sort the buffer and drop the four most dispersed values
            adc_buf.sort_unstable();
            
            //average the remaining 8 values
            let sample = average(&adc_buf[2..10]);
            
            //update the global buffer with the new sample
            let buf = BUF.borrow(cs).get();
            let new_buf = circular(&buf, sample);
            BUF.borrow(cs).replace(new_buf);

             //get the average of the current global buffer
            let avg_sample = average(&new_buf);
                        
            //ADC reading converted to milivolts, then to Celsius degrees
            //the common formula is (milivolts - 500) / 10
            //10mV per Celsius degree with 500 mV offset

            let voltage = avg_sample as f32 * FACTOR; 

            let celsius = (voltage - 500.0) / 10.0; 

            let mut fahrenheit = celsius * 9.0;
            fahrenheit /= 5.0;
            fahrenheit += 32.0;

            //as we want to get the tenths of the degree and display them easily
            //we multiply the results by 10

            TEMP_C.borrow(cs).replace((celsius * 10.0) as i16);
            TEMP_F.borrow(cs).replace((fahrenheit * 10.0) as i16);
        }
    });
}


#[interrupt]

fn TIM2() {

    free(|cs| {
        
                
        stm32::NVIC::unpend(Interrupt::TIM2);
        if let (&mut Some(ref mut button), Some(ref mut tim2)) = (
            BUTTON.borrow(cs).borrow_mut().deref_mut(),
            TIMER_TIM2.borrow(cs).borrow_mut().deref_mut()) {
            tim2.clear_interrupt(Event::TimeOut);

            let state1 = STATE1.borrow(cs).get(); //get state1
            let state2 = STATE2.borrow(cs).get(); //get state2
            let flag = FLAG.borrow(cs).get(); //get the flag
            let current = button.is_low().unwrap(); //button pressed?

            //if (current == true) && (current == previous) { //if button pressed and previous state was true
            if (current == false) && (state1 == true) && (state2 == true) { //if button NOT pressed and both previous states were true
                FLAG.borrow(cs).replace(!flag); //flip the flag
                }
                
            //PREVIOUS.borrow(cs).replace(current); //update the previous state
            STATE1.borrow(cs).replace(state2); //shift the previous state into the past
            STATE2.borrow(cs).replace(current);

            }


        });

    }    




fn formatter(buf: &mut ArrayString<[u8; 8]>, val: i16, unit: char) {   
    // helper function for the display    
    // takes a mutable text buffer, value and unit symbol as arguments
    // default sign is + (43 in ASCII)
    // in order to correctly handle negative values, their sign has to be reversed before splitting into digits
    
    let mut sign: char = 43 as char; 
    
    if val < 0 {
        sign = 45 as char;
    };
    
    let mut new_val = val;
    if val < 0 {
        new_val *= -1; 
    }

    let tenths = new_val%10;
    let singles = (new_val/10)%10;
    let tens = (new_val/100)%10;
    let hundreds = (new_val/1000)%10;
    
    //correctly handle values with only one or two digits, e.g. +100.5 F, -23.4 C, +7.5 F

    if (hundreds == 0) && (tens == 0) {
        fmt::write(buf, format_args!("{}  {}.{} {}", sign, singles as u8, tenths as u8, unit)).unwrap();
    } 
    else if hundreds == 0 {
        fmt::write(buf, format_args!("{} {}{}.{} {}", sign, tens as u8, singles as u8, tenths as u8, unit)).unwrap();
    }
    else {
        fmt::write(buf, format_args!("{}{}{}{}.{} {}", sign, hundreds as u8, tens as u8, singles as u8, tenths as u8, unit)).unwrap();
    }

}


fn circular(buf: &[u16;8], val: u16) -> [u16;8] {

    //simple circular buffer, first in first out
    let mut new_buf: [u16;8] = [0u16;8];
    for i in 0..7 {
        new_buf[i] = buf[i+1];
    }
    new_buf[7] = val;
    return new_buf
}


//simple average function, averages the 8 values by shifting right by 3 bits

fn average(buf: &[u16]) -> u16 {

    let mut total: u16 = 0u16;
    for i in buf.iter() {
        total += i;
    }
    return total >> 3;
}
