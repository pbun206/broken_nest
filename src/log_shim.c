#include <stdarg.h>
#include <stdio.h>
#include <stdint.h>

// Rust callback: receives formatted message
extern void lv2_log_shim_callback(void *handle, uint32_t type_, const char *msg);

int lv2_log_printf(void *handle, uint32_t type_, const char *fmt, ...) {
    char buf[1024];
    va_list args;
    va_start(args, fmt);
    int n = vsnprintf(buf, sizeof(buf), fmt, args);
    va_end(args);
    if (n > 0) {
        lv2_log_shim_callback(handle, type_, buf);
    }
    return n;
}

int lv2_log_vprintf(void *handle, uint32_t type_, const char *fmt, va_list ap) {
    char buf[1024];
    int n = vsnprintf(buf, sizeof(buf), fmt, ap);
    if (n > 0) {
        lv2_log_shim_callback(handle, type_, buf);
    }
    return n;
}
