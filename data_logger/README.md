# data_logger

TCP-сервер для приёма зашифрованных данных с ESP32 и записи в PostgreSQL/TimescaleDB.

Миграции БД управляются из `meteo_display_backend` — здесь их нет намеренно.

## SensorData

Все поля данных (pressure, temp, co2, humidity) — `Option`, потому что отдельные датчики могут быть недоступны в момент измерения. Это согласовано во всех компонентах: firmware, data_logger, БД (nullable колонки), display_backend и frontend.
