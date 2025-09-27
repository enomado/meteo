#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(unused_mut)]

use std::net::UdpSocket;

fn main() -> std::io::Result<()> {
    // Привязываем сервер к локальному порту 8080
    let socket = UdpSocket::bind("0.0.0.0:1234")?;
    println!("UDP сервер запущен на 0.0.0.0:1234");

    let mut buf = [0u8; 1024]; // буфер для входящих данных
    loop {
        // Получаем данные
        let (amt, src) = socket.recv_from(&mut buf)?;
        let data = &buf[..amt];

        // Преобразуем в строку для удобства
        if let Ok(s) = std::str::from_utf8(data) {
            println!("Получено от {}: {}", src, s);
        } else {
            println!("Получено от {}: {:?} (не UTF-8)", src, data);
        }
    }
}
