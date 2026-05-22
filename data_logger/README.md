# data_logger

TCP-сервер для приёма зашифрованных данных с ESP32 и записи в PostgreSQL/TimescaleDB.

Миграции БД управляются из `meteo_display_backend` — здесь их нет намеренно.

## SensorData

Поля группируются по источнику: `Option<BaroReading{pressure,temp}>` для BMP390 и `Option<ScdReading{co2,humidity,temp}>` для SCD41 — поля одного датчика всегда читаются одним вызовом, поэтому либо все Some, либо все None. На входе в БД распаковывается обратно в плоские nullable колонки `sensor(pressure, temp, co2, humidity, scd_temp)` — display_backend/frontend схему не меняют.
