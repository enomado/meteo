CREATE MATERIALIZED VIEW IF NOT EXISTS sensor_fivenum
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
