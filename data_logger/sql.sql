-- создать базу (если ещё нет)
CREATE DATABASE meteo;

-- подключаемся к ней
\c meteo;

-- включаем расширение TimescaleDB
CREATE EXTENSION IF NOT EXISTS timescaledb;

-- создаём таблицу
CREATE TABLE sensor (
    time TIMESTAMPTZ       NOT NULL,
    pressure REAL,
    temp REAL,
    co2 INTEGER,
    humidity REAL
);

-- превращаем её в hypertable
SELECT create_hypertable('sensor', 'time');

---------


-- индекс для ускорения поиска по времени
CREATE INDEX ON sensor (time DESC);

-- индекс для фильтрации по температуре или давлению (опционально)
CREATE INDEX ON sensor (pressure);
CREATE INDEX ON sensor (temp);

-- индексы для co2 и humidity
CREATE INDEX ON sensor (co2);
CREATE INDEX ON sensor (humidity);

-- создаём continuous aggregate для усреднения
CREATE MATERIALIZED VIEW sensor_fivenum
WITH (timescaledb.continuous) AS
SELECT
    time_bucket('1 minute', time) AS bucket,
    -- temp
    min(temp)::real AS temp_min,
    percentile_cont(0.25) WITHIN GROUP (ORDER BY temp)::real AS temp_q1,
    percentile_cont(0.5)  WITHIN GROUP (ORDER BY temp)::real AS temp_median,
    percentile_cont(0.75) WITHIN GROUP (ORDER BY temp)::real AS temp_q3,
    max(temp)::real AS temp_max,
    -- pressure
    min(pressure)::real AS pressure_min,
    percentile_cont(0.25) WITHIN GROUP (ORDER BY pressure)::real AS pressure_q1,
    percentile_cont(0.5)  WITHIN GROUP (ORDER BY pressure)::real AS pressure_median,
    percentile_cont(0.75) WITHIN GROUP (ORDER BY pressure)::real AS pressure_q3,
    max(pressure)::real AS pressure_max,
    -- co2
    min(co2) AS co2_min,
    percentile_cont(0.5) WITHIN GROUP (ORDER BY co2)::integer AS co2_median,
    max(co2) AS co2_max,
    -- humidity
    min(humidity)::real AS humidity_min,
    percentile_cont(0.5) WITHIN GROUP (ORDER BY humidity)::real AS humidity_median,
    max(humidity)::real AS humidity_max
FROM sensor
GROUP BY bucket;