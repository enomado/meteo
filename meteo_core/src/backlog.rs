//! Очередь показаний между sensor-таской и отправителем.

use heapless::spsc::Queue;

use crate::wire::SensorData;

/// Очередь показаний к отправке. heapless `Queue<_, N>` вмещает N−1 элемент ⇒
/// 59 показаний ≈ 30 мин при цикле ~30с; дальше вытесняются самые старые.
pub type SensorQueue = Queue<SensorData, 60>;

/// Кладёт показание. Очередь полна ⇒ вытесняет самое старое и возвращает его:
/// потеря учтена, а не молча проглочена.
pub fn push_evicting(queue: &mut SensorQueue, reading: SensorData) -> Option<SensorData> {
    match queue.enqueue(reading) {
        Ok(()) => None,
        Err(reading) => {
            // Очередь полна ⇒ в ней есть хотя бы один элемент, и после dequeue
            // ровно одно место свободно: оба вызова не могут не сработать.
            let evicted = queue.dequeue().expect("full queue has at least one entry");
            queue.enqueue(reading).expect("dequeue freed exactly one slot");
            Some(evicted)
        }
    }
}
