#[cfg(feature = "compression")]
mod compression_memory_pool_test {
    use sedsprintf_rs::config::{DataEndpoint, DataType};
    use sedsprintf_rs::packet::Packet;
    use sedsprintf_rs::serialize;

    use std::alloc::{GlobalAlloc, Layout, System};
    use std::ptr::null_mut;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const HDR_WORDS: usize = 4;
    const HDR_RAW_OFF: usize = 0;
    const HDR_REQ_OFF: usize = 1;
    const HDR_TOTAL_OFF: usize = 2;
    const HDR_ALIGN_OFF: usize = 3;

    struct LimitedAlloc;

    static ENABLE_LIMIT: AtomicBool = AtomicBool::new(false);
    static LIMIT_BYTES: AtomicUsize = AtomicUsize::new(usize::MAX);
    static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
    static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

    #[global_allocator]
    static GLOBAL: LimitedAlloc = LimitedAlloc;

    #[inline]
    fn align_up(addr: usize, align: usize) -> usize {
        (addr + (align - 1)) & !(align - 1)
    }

    #[inline]
    fn update_peak(live: usize) {
        let mut cur = PEAK_BYTES.load(Ordering::Relaxed);
        while live > cur {
            match PEAK_BYTES.compare_exchange_weak(cur, live, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
    }

    #[inline]
    fn try_reserve(bytes: usize) -> bool {
        loop {
            let live = LIVE_BYTES.load(Ordering::Relaxed);
            let next = live.saturating_add(bytes);
            if ENABLE_LIMIT.load(Ordering::Relaxed) && next > LIMIT_BYTES.load(Ordering::Relaxed) {
                return false;
            }
            if LIVE_BYTES
                .compare_exchange_weak(live, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                update_peak(next);
                return true;
            }
        }
    }

    #[inline]
    fn release(bytes: usize) {
        LIVE_BYTES.fetch_sub(bytes, Ordering::Relaxed);
    }

    unsafe impl GlobalAlloc for LimitedAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let req = layout.size().max(1);
            let align = layout.align().max(std::mem::align_of::<usize>());
            let hdr_sz = HDR_WORDS * std::mem::size_of::<usize>();
            let total = req.saturating_add(align).saturating_add(hdr_sz);

            if !try_reserve(req) {
                return null_mut();
            }

            let raw_layout = match Layout::from_size_align(total, align) {
                Ok(v) => v,
                Err(_) => {
                    release(req);
                    return null_mut();
                }
            };

            let raw = unsafe { System.alloc(raw_layout) };
            if raw.is_null() {
                release(req);
                return null_mut();
            }

            let aligned = align_up(raw as usize + hdr_sz, align) as *mut u8;
            let hdr = aligned as *mut usize;
            unsafe {
                *hdr.sub(HDR_RAW_OFF + 1) = raw as usize;
                *hdr.sub(HDR_REQ_OFF + 1) = req;
                *hdr.sub(HDR_TOTAL_OFF + 1) = total;
                *hdr.sub(HDR_ALIGN_OFF + 1) = align;
            }

            aligned
        }

        unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
            if ptr.is_null() {
                return;
            }
            let hdr = ptr as *mut usize;
            let raw = unsafe { *hdr.sub(HDR_RAW_OFF + 1) as *mut u8 };
            let req = unsafe { *hdr.sub(HDR_REQ_OFF + 1) };
            let total = unsafe { *hdr.sub(HDR_TOTAL_OFF + 1) };
            let align = unsafe { *hdr.sub(HDR_ALIGN_OFF + 1) };

            release(req);
            let raw_layout = Layout::from_size_align(total, align).expect("bad stored layout");
            unsafe { System.dealloc(raw, raw_layout) };
        }

        unsafe fn realloc(&self, ptr: *mut u8, old_layout: Layout, new_size: usize) -> *mut u8 {
            let new_layout = match Layout::from_size_align(new_size.max(1), old_layout.align()) {
                Ok(v) => v,
                Err(_) => return null_mut(),
            };
            let new_ptr = unsafe { self.alloc(new_layout) };
            if new_ptr.is_null() {
                return null_mut();
            }
            let copy_len = old_layout.size().min(new_size);
            unsafe { std::ptr::copy_nonoverlapping(ptr, new_ptr, copy_len) };
            unsafe { self.dealloc(ptr, old_layout) };
            new_ptr
        }
    }

    fn enable_limit(limit: usize) {
        LIVE_BYTES.store(0, Ordering::Relaxed);
        PEAK_BYTES.store(0, Ordering::Relaxed);
        LIMIT_BYTES.store(limit, Ordering::Relaxed);
        ENABLE_LIMIT.store(true, Ordering::Relaxed);
    }

    fn disable_limit() {
        ENABLE_LIMIT.store(false, Ordering::Relaxed);
    }

    fn make_packet(payload: &[u8], ts: u64) -> Packet {
        Packet::new(
            DataType::MessageData,
            &[DataEndpoint::SdCard],
            "POOL_TEST",
            ts,
            Arc::<[u8]>::from(payload),
        )
        .expect("packet build failed")
    }

    #[test]
    fn compression_path_is_stable_under_limited_memory_pool() {
        // Warm-up outside the cap to initialize one-time internals.
        let warm = make_packet(&[b'W'; 128], 0);
        let wire = serialize::serialize_packet(&warm);
        let _ = serialize::deserialize_packet(&wire).expect("warm-up deserialize failed");

        // Keep this tight enough to catch regressions, but high enough for stable test runtime.
        let limit_bytes = 32 * 1024usize;
        enable_limit(limit_bytes);

        for i in 0..1200u64 {
            let payload = if i % 2 == 0 {
                vec![b'R'; 192]
            } else {
                let mut v = Vec::with_capacity(192);
                for j in 0..192u16 {
                    v.push(32u8 + (((i as u16 + j) as u8) % 95));
                }
                v
            };

            let pkt = make_packet(&payload, i + 1);
            let wire = serialize::serialize_packet(&pkt);
            let decoded = serialize::deserialize_packet(&wire).expect("deserialize failed");
            assert_eq!(decoded.payload(), payload.as_slice());
        }

        disable_limit();
        let peak = PEAK_BYTES.load(Ordering::Relaxed);
        assert!(
            peak <= limit_bytes,
            "allocator exceeded pool cap: peak={peak} limit={limit_bytes}"
        );
    }
}
