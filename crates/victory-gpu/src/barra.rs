//! P1nkman: a barra de título da janela do jogo, em português, como nas
//! versões p19–p23 (adaptada do `painel.rs` da p23, sem painel na tela):
//! quadros por segundo, ms por quadro e o pior quadro do último segundo, a
//! ocupação da placa e do processador, a memória do processo, draws, o total
//! de quadros, há quanto tempo o jogo está aberto e, quando nenhum quadro
//! chega, há quanto tempo está sem quadro.
//!
//! O texto é posto por uma thread nossa a cada 200 ms (pôr o texto espera a
//! thread da janela, que é do título): a apresentação nunca espera, e o
//! relógio e o "sem quadro" andam mesmo com o jogo parado.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::SetWindowTextW;
use windows::core::PCWSTR;

/// Um segundo do medidor, por quadro.
#[derive(Clone, Copy, Default)]
pub(crate) struct Figures {
    pub(crate) fps: f64,
    pub(crate) ms: f64,
    /// O quadro mais longo do segundo, ms.
    pub(crate) worst_ms: f64,
    pub(crate) draws: f64,
    /// A placa ocupada com o que mandamos, pela conta do relógio dela (%).
    pub(crate) gpu_pct: f64,
}

/// Quadros apresentados desde o início, e o instante do último (ms desde
/// [`start`]).
static FRAMES: AtomicU64 = AtomicU64::new(0);
static LAST_FRAME_MS: AtomicU64 = AtomicU64::new(0);

fn start() -> Instant {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// A cada quadro apresentado.
pub(crate) fn presented() {
    FRAMES.fetch_add(1, Relaxed);
    LAST_FRAME_MS.store(start().elapsed().as_millis() as u64, Relaxed);
}

/// Uma vez por segundo, pelo medidor: a janela, o texto que ela tinha e os
/// números do segundo.
pub(crate) fn second(hwnd: HWND, own: &[u16], figures: Figures) {
    static PENDING: Mutex<Option<(usize, String, Figures)>> = Mutex::new(None);
    static SETTER: std::sync::Once = std::sync::Once::new();
    let own = String::from_utf16_lossy(own);
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, own, figures));
    SETTER.call_once(|| {
        let _ = std::thread::Builder::new().name("victory-caption".into()).spawn(|| {
            let mut newest: Option<(usize, String, Figures)> = None;
            let mut shown = String::new();
            // Uso do processador: (instante, tempo de CPU do processo em µs).
            let mut cpu_last: Option<(Instant, u64)> = None;
            let mut cpu_pct = 0.0;
            let mut cpu_at = Instant::now();
            loop {
                if let Some(next) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    newest = Some(next);
                }
                // O processador a cada segundo (a conta precisa de um intervalo).
                if cpu_at.elapsed() >= Duration::from_secs(1) || cpu_last.is_none() {
                    cpu_at = Instant::now();
                    if let Some(us) = process_cpu_us() {
                        if let Some((at, before)) = cpu_last {
                            cpu_pct = cpu_share(us.saturating_sub(before), cpu_at.duration_since(at), logical_processors());
                        }
                        cpu_last = Some((cpu_at, us));
                    }
                }
                if let Some((hwnd, own, f)) = &newest {
                    let now_ms = start().elapsed().as_millis() as u64;
                    let last = LAST_FRAME_MS.load(Relaxed);
                    let without = (last > 0).then(|| Duration::from_millis(now_ms.saturating_sub(last)));
                    let text = caption(
                        own,
                        f,
                        cpu_pct,
                        process_working_set().unwrap_or(0),
                        FRAMES.load(Relaxed),
                        process_uptime(),
                        without,
                    );
                    if text != shown {
                        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
                        // SAFETY: a window handle and a NUL-terminated text.
                        unsafe {
                            let _ = SetWindowTextW(HWND(*hwnd as *mut _), PCWSTR(wide.as_ptr()));
                        }
                        shown = text;
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
    });
}

/// A parte do processador de toda a máquina que o processo usou: `used` de
/// CPU em µs, num intervalo `over`, com `processors` lógicos (%).
fn cpu_share(used_us: u64, over: Duration, processors: u32) -> f64 {
    let seconds = over.as_secs_f64().max(1e-3);
    (used_us as f64 / 1e6 / seconds / f64::from(processors.max(1)) * 100.0).clamp(0.0, 100.0)
}

/// O texto da barra.
fn caption(own: &str, f: &Figures, cpu_pct: f64, ram: u64, frames: u64, up: Duration, without: Option<Duration>) -> String {
    let own = if own.trim().is_empty() { "P1nkman" } else { own.trim() };
    let ram_gb = ram as f64 / (1u64 << 30) as f64;
    let mut text = format!(
        "{own} | {:.0} FPS {:.1} ms/quadro (pior {:.0} ms) | GPU {:.0}% CPU {:.0}% RAM {:.1} GB | {:.0} draws | quadro {} | ligado há {}",
        f.fps,
        f.ms,
        f.worst_ms,
        f.gpu_pct,
        cpu_pct,
        ram_gb,
        f.draws,
        frames,
        clock(up)
    );
    // Um segundo inteiro sem quadro (o medidor fala a cada segundo, então
    // os números acima são de antes).
    if let Some(w) = without
        && w >= Duration::from_millis(1500)
    {
        text.push_str(&format!(" | SEM QUADRO há {:.1} s", w.as_secs_f64()));
    }
    text
}

/// m:ss, ou h:mm:ss.
fn clock(up: Duration) -> String {
    let s = up.as_secs();
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h == 0 { format!("{m}:{s:02}") } else { format!("{h}:{m:02}:{s:02}") }
}

fn process_cpu_us() -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    // SAFETY: plain C structs, zero is a valid value for each.
    let mut times = unsafe { std::mem::zeroed::<[FILETIME; 4]>() };
    let [creation, exit, kernel, user] = &mut times;
    // SAFETY: the current-process pseudo handle and four writable FILETIMEs.
    if unsafe { GetProcessTimes(GetCurrentProcess(), creation, exit, kernel, user) } == 0 {
        return None;
    }
    let ticks = |f: &FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    Some((ticks(kernel) + ticks(user)) / 10)
}

/// The process's working set in bytes.
fn process_working_set() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: a plain C struct, zero is a valid value.
    let mut counters = unsafe { std::mem::zeroed::<PROCESS_MEMORY_COUNTERS>() };
    // SAFETY: the current-process pseudo handle and a counter of the size passed.
    let ok = unsafe {
        GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32)
    };
    (ok != 0).then_some(counters.WorkingSetSize as u64)
}

/// The machine's logical processors.
fn logical_processors() -> u32 {
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    // SAFETY: the call fills the plain struct.
    let mut info = unsafe { std::mem::zeroed::<SYSTEM_INFO>() };
    // SAFETY: as above.
    unsafe { GetSystemInfo(&mut info) };
    info.dwNumberOfProcessors.max(1)
}

/// How long ago the kernel created this process.
fn process_age() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    // A FILETIME counts 100 ns from 1601; the Unix epoch is this many later.
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    // SAFETY: plain C structs, zero is a valid value for each.
    let mut times = unsafe { std::mem::zeroed::<[FILETIME; 4]>() };
    let [creation, exit, kernel, user] = &mut times;
    // SAFETY: the current-process pseudo handle and four writable FILETIMEs.
    if unsafe { GetProcessTimes(GetCurrentProcess(), creation, exit, kernel, user) } == 0 {
        return None;
    }
    let ticks = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    let created = Duration::from_nanos(ticks.checked_sub(UNIX_EPOCH_TICKS)?.checked_mul(100)?);
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.checked_sub(created)
}

/// How long the process has been running: its age when first asked, plus the
/// time since, so the wall clock being set does not move it.
fn process_uptime() -> Duration {
    static FIRST: std::sync::OnceLock<(Instant, Duration)> = std::sync::OnceLock::new();
    let (at, age) = *FIRST.get_or_init(|| (Instant::now(), process_age().unwrap_or_default()));
    age + at.elapsed()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn figures() -> Figures {
        Figures { fps: 12.4, ms: 81.04, worst_ms: 119.6, draws: 4713.2, gpu_pct: 44.6 }
    }

    #[test]
    fn a_barra_tem_todos_os_numeros() {
        let text = caption("P1nkman", &figures(), 37.6, 10 << 30, 12345, Duration::from_secs(754), Some(Duration::from_millis(80)));
        assert_eq!(
            text,
            "P1nkman | 12 FPS 81.0 ms/quadro (pior 120 ms) | GPU 45% CPU 38% RAM 10.0 GB | 4713 draws | quadro 12345 | ligado há 12:34"
        );
    }

    #[test]
    fn sem_quadro_aparece_so_depois_de_um_segundo_e_meio() {
        let f = figures();
        let up = Duration::from_secs(3723);
        let at = |ms| caption("", &f, 0.0, 0, 9, up, Some(Duration::from_millis(ms)));
        assert!(!at(1400).contains("SEM QUADRO"));
        assert!(at(3200).ends_with("ligado há 1:02:03 | SEM QUADRO há 3.2 s"), "{}", at(3200));
        assert!(at(1400).starts_with("P1nkman | "));
        assert!(!caption("P1nkman", &f, 0.0, 0, 0, up, None).contains("SEM QUADRO"));
    }

    #[test]
    fn a_cpu_e_a_parte_de_toda_a_maquina() {
        // 4 s de CPU em 1 s, 16 processadores: 25%.
        assert!((cpu_share(4_000_000, Duration::from_secs(1), 16) - 25.0).abs() < 1e-9);
        assert_eq!(cpu_share(u64::MAX / 2, Duration::from_secs(1), 1), 100.0);
    }
}
