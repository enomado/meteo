-- создать базу (если ещё нет)
CREATE DATABASE meteo;

-- подключаемся к ней
\c meteo;

-- включаем расширение TimescaleDB
CREATE EXTENSION IF NOT EXISTS timescaledb;

-- создаём таблицу
CREATE TABLE sensor (
    time TIMESTAMPTZ       NOT NULL,
    pressure REAL          NOT NULL,
    temp REAL              NOT NULL
);

-- превращаем её в hypertable
SELECT create_hypertable('sensor', 'time');

---------


-- индекс для ускорения поиска по времени
CREATE INDEX ON sensor (time DESC);

-- индекс для фильтрации по температуре или давлению (опционально)
CREATE INDEX ON sensor (pressure);
CREATE INDEX ON sensor (temp);

-- создаём continuous aggregate для усреднения 
CREATE MATERIALIZED VIEW sensor_fivenum
WITH (timescaledb.continuous) AS
SELECT
    time_bucket('1 minute', time) AS bucket,
    min(temp)::real AS temp_min,
    percentile_cont(0.25) WITHIN GROUP (ORDER BY temp)::real AS temp_q1,
    percentile_cont(0.5)  WITHIN GROUP (ORDER BY temp)::real AS temp_median,
    percentile_cont(0.75) WITHIN GROUP (ORDER BY temp)::real AS temp_q3,
    max(temp)::real AS temp_max,
    min(pressure)::real AS pressure_min,
    percentile_cont(0.25) WITHIN GROUP (ORDER BY pressure)::real AS pressure_q1,
    percentile_cont(0.5)  WITHIN GROUP (ORDER BY pressure)::real AS pressure_median,
    percentile_cont(0.75) WITHIN GROUP (ORDER BY pressure)::real AS pressure_q3,
    max(pressure)::real AS pressure_max
FROM sensor
GROUP BY bucket

-- SELECT add_continuous_aggregate_policy(
--    'sensor_avg_1m',
--    start_offset => INTERVAL '10 seconds',
--    end_offset => INTERVAL '0 seconds',
--    schedule_interval => INTERVAL '1 minute'
-- );