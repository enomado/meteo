https://docs.espressif.com/projects/esp-hardware-design-guidelines/en/latest/esp32c3/schematic-checklist.html#fig-rf-tuning


https://www.espressif.com/sites/default/files/documentation/esp32-c3_technical_reference_manual_en.pdf#iomuxgpio


https://botland.store/withdrawn-products/21026-esp-c3-32s-kit-wifi-bluetooth-development-board-with-esp-c3-32s-module.html

SPID = MOSI = data out
SPIQ = MISO = data in

20 SPICS0 SPICS0 / IO14
21 SPICLK SPICLK / IO15
22 SPIQ SPIQ / IO17           MOSI
23 SPID SPID / IO16           MISO

Плата (ESP32)	Устройство
MOSI	SDI
MISO	SDO
SCK	SCK
GPIO (CS)	CS

<!-- 13 14 не трогать - для отладки -->