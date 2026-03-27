ALTER TABLE sensor
    ALTER COLUMN pressure DROP NOT NULL;

ALTER TABLE sensor
    ALTER COLUMN temp DROP NOT NULL;

ALTER TABLE sensor
    ADD COLUMN IF NOT EXISTS co2 INTEGER,
    ADD COLUMN IF NOT EXISTS humidity REAL;

CREATE INDEX IF NOT EXISTS sensor_co2_idx ON sensor (co2);
CREATE INDEX IF NOT EXISTS sensor_humidity_idx ON sensor (humidity);
