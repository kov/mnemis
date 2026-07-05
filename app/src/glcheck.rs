//! Startup workaround for limited/software OpenGL stacks on Linux.
//!
//! Some environments (notably VMs whose guest GL driver caps out at OpenGL
//! 3.2 — e.g. `Virtio-GPU Venus`/zink) can't compile the GLSL 3.30 shaders
//! WebKitGTK's accelerated compositor emits. Every shader fails, the webview
//! never paints, and the log fills with:
//!
//! ```text
//! Gdk-WARNING: Compile failure in vertex shader:
//! error: GLSL 3.30 is not supported. Supported versions are: ... 1.50 ...
//! ```
//!
//! When we can *positively* determine the desktop GL version is below 3.3 we
//! set `WEBKIT_DISABLE_COMPOSITING_MODE=1`, which makes WebKit fall back to
//! software compositing (cairo) instead. Capable GPUs are left untouched so
//! they keep hardware acceleration, and a value the user set themselves always
//! wins. If the probe can't reach a conclusion we change nothing — better to
//! leave acceleration on than to regress a working machine on a hunch.

use std::ffi::{CStr, c_char};

use tracing::{debug, info};

/// GL enum for `glGetString(GL_VERSION)`.
const GL_VERSION: u32 = 0x1F02;

/// Set `WEBKIT_DISABLE_COMPOSITING_MODE=1` when the desktop GL driver is too
/// old for WebKit's compositor. Must run at the very top of `main()`, before
/// GTK/WebKit initialize (they read the variable at compositor setup) and
/// before any other threads are spawned (edition-2024 `set_var` is unsafe
/// under concurrent `getenv`).
pub fn apply_webkit_compositing_workaround() {
    // An explicit user setting wins either way — never override it.
    if std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE").is_some() {
        return;
    }

    match desktop_gl_supports_3_3() {
        Some(true) => {} // Capable GPU: keep hardware compositing.
        Some(false) => {
            info!(
                "desktop GL < 3.3: disabling WebKit GL compositing \
                 (WEBKIT_DISABLE_COMPOSITING_MODE=1) so the webview renders"
            );
            // SAFETY: called before any threads are spawned in `main()`, so no
            // other thread can be reading the environment concurrently.
            unsafe { std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1") };
        }
        None => debug!("could not probe desktop GL version; leaving WebKit compositing unchanged"),
    }
}

/// `Some(true)`/`Some(false)` if the max desktop GL version is at/below 3.3;
/// `None` if the version couldn't be determined.
fn desktop_gl_supports_3_3() -> Option<bool> {
    // Run the EGL probe on a throwaway thread so its thread-local current
    // context/display state can never leak into the main thread that WebKit
    // later initializes on. The join also guarantees the probe thread is gone
    // before the caller touches the environment.
    std::thread::Builder::new()
        .name("gl-probe".into())
        .spawn(probe)
        .ok()?
        .join()
        .ok()
        .flatten()
}

/// Create a throwaway desktop-GL context and read its `GL_VERSION`. Returns
/// `Some(true)` if it advertises >= 3.3, `Some(false)` if below, `None` on any
/// failure to determine.
fn probe() -> Option<bool> {
    use khronos_egl as egl;

    // SAFETY: `load_required` only dlopen's libEGL.so(.1).
    let egl = unsafe { egl::DynamicInstance::<egl::EGL1_4>::load_required() }.ok()?;
    // SAFETY: DEFAULT_DISPLAY is the documented null native display handle.
    let display = unsafe { egl.get_display(egl::DEFAULT_DISPLAY) }?;
    egl.initialize(display).ok()?;
    // Bind desktop OpenGL — the same API WebKit's compositor uses (and the one
    // whose 3.30 shaders were failing). GLES availability is irrelevant here.
    egl.bind_api(egl::OPENGL_API).ok()?;

    let config = egl
        .choose_first_config(
            display,
            &[
                egl::SURFACE_TYPE,
                egl::PBUFFER_BIT,
                egl::RENDERABLE_TYPE,
                egl::OPENGL_BIT,
                egl::NONE,
            ],
        )
        .ok()??;

    // No version attributes: the driver hands back its highest-supported
    // compatibility context, whose GL_VERSION is the ceiling we care about.
    let context = egl
        .create_context(display, config, None, &[egl::NONE])
        .ok()?;
    // A 1x1 pbuffer avoids depending on the surfaceless-context extension.
    let surface = egl
        .create_pbuffer_surface(display, config, &[egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE])
        .ok()?;
    egl.make_current(display, Some(surface), Some(surface), Some(context))
        .ok()?;

    // glGetString is a core entry point; eglGetProcAddress resolves it once a
    // context is current (guaranteed on Mesa, which is who this workaround is
    // for).
    let version = egl.get_proc_address("glGetString").and_then(|raw| {
        // SAFETY: transmute the untyped fn pointer to glGetString's signature.
        let gl_get_string: extern "system" fn(u32) -> *const c_char =
            unsafe { std::mem::transmute(raw) };
        let ptr = gl_get_string(GL_VERSION);
        if ptr.is_null() {
            None
        } else {
            // SAFETY: Mesa returns a static NUL-terminated ASCII string.
            unsafe { CStr::from_ptr(ptr) }
                .to_str()
                .ok()
                .map(str::to_owned)
        }
    });

    // Best-effort teardown; we're on a throwaway thread, so errors don't matter.
    let _ = egl.make_current(display, None, None, None);
    let _ = egl.destroy_surface(display, surface);
    let _ = egl.destroy_context(display, context);
    let _ = egl.terminate(display);

    let version = version?;
    debug!(gl_version = %version, "probed desktop GL version");
    Some(gl_version_at_least_3_3(&version))
}

/// Parse the leading `MAJOR.MINOR` of a `GL_VERSION` string and test it against
/// 3.3. The string looks like `"3.2 (Compatibility Profile) Mesa 26.1.3"`.
fn gl_version_at_least_3_3(version: &str) -> bool {
    let head = version.split_whitespace().next().unwrap_or("");
    let mut parts = head.split('.');
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor) >= (3, 3)
}

#[cfg(test)]
mod tests {
    use super::gl_version_at_least_3_3;

    #[test]
    fn parses_gl_version_strings() {
        assert!(!gl_version_at_least_3_3(
            "3.2 (Compatibility Profile) Mesa 26.1.3"
        ));
        assert!(!gl_version_at_least_3_3("3.0 Mesa 20.0"));
        assert!(gl_version_at_least_3_3("3.3 (Core Profile) Mesa 24.0"));
        assert!(gl_version_at_least_3_3(
            "4.6 (Compatibility Profile) Mesa 24.0"
        ));
        assert!(gl_version_at_least_3_3("4.6.0 NVIDIA 550.0"));
        assert!(!gl_version_at_least_3_3("garbage"));
    }
}
