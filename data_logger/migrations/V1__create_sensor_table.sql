CREATE EXTENSION IF NOT EXISTS timescaledb;

CREATE TABLE IF NOT EXISTS sensor (
    time TIMESTAMPTZ NOT NULL,
    pressure REAL,
    temp REAL
);

SELECT create_hypertable('sensor', 'time', if_not_exists => TRUE);

CREATE INDEX IF NOT EXISTS sensor_time_desc_idx ON sensor (time DESC);
CREATE INDEX IF NOT EXISTS sensor_pressure_idx ON sensor (pressure);
CREATE INDEX IF NOT EXISTS sensor_temp_idx ON sensor (temp);
