use spin::Mutex;
use syscall::Pio;

use crate::serial_16550::SerialPort;

pub static COM1: Mutex<SerialPort<Pio<u8>>> = Mutex::new(SerialPort::<Pio<u8>>::new(0x3F8));
