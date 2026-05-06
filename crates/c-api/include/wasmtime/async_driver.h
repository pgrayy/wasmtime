/**
 * \file wasmtime/async_driver.h
 *
 * \brief External event loop integration for async Wasmtime.
 *
 * This API enables embedding Wasmtime's async runtime within a foreign event
 * loop (e.g., Python's asyncio, libuv, GLib). The host monitors a single file
 * descriptor and calls \ref wasmtime_async_driver_poll_once when it becomes
 * readable.
 *
 * ## Platform strategies
 *
 * ### Unix (macOS, Linux)
 *
 * Single-threaded. The host monitors one fd (the I/O reactor's kqueue/epoll
 * fd) via its own event loop. When the fd is readable, the host calls
 * \ref wasmtime_async_driver_poll_once which runs one non-blocking iteration
 * of the internal scheduler. No background threads are required.
 *
 * ### Windows
 *
 * The async runtime runs on a background thread. The host provides a
 * notification socket; the runtime writes to it when tasks complete. The host
 * monitors the socket via its event loop (e.g., IOCP).
 *
 * ## Usage
 *
 * 1. Create a driver with \ref wasmtime_async_driver_new
 * 2. Get the fd with \ref wasmtime_async_driver_io_fd (Unix) or set up
 *    notification with \ref wasmtime_async_driver_set_notify_socket (Windows)
 * 3. Monitor the fd in your event loop
 * 4. Call \ref wasmtime_async_driver_poll_once when the fd is readable
 * 5. Poll task status with \ref wasmtime_async_driver_task_poll
 * 6. Retrieve results with \ref wasmtime_async_driver_task_get_response /
 *    \ref wasmtime_async_driver_task_get_error
 * 7. Free strings with \ref wasmtime_async_driver_free_string
 * 8. Delete the driver with \ref wasmtime_async_driver_delete
 */

#ifndef WASMTIME_ASYNC_DRIVER_H
#define WASMTIME_ASYNC_DRIVER_H

#include <stdint.h>
#include <wasm.h>
#include <wasmtime/error.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * \brief Opaque handle to an async event loop driver.
 */
typedef struct wasmtime_async_driver wasmtime_async_driver_t;

/**
 * \brief Task completion status codes.
 */
enum wasmtime_async_task_status {
  /** Task is still running. */
  WASMTIME_ASYNC_TASK_PENDING = 0,
  /** Task completed successfully. */
  WASMTIME_ASYNC_TASK_OK = 1,
  /** Task completed with an error. */
  WASMTIME_ASYNC_TASK_ERROR = -1,
};

/**
 * \brief Create a new async driver.
 *
 * On Unix, this creates a current-thread Tokio runtime. On Windows, this
 * also starts a background thread to drive the runtime.
 *
 * The returned driver must be freed with \ref wasmtime_async_driver_delete.
 *
 * \return A new driver, or NULL on failure.
 */
WASM_API_EXTERN wasmtime_async_driver_t *wasmtime_async_driver_new(void);

/**
 * \brief Delete an async driver and release its resources.
 *
 * Pending tasks are cancelled. Completed results that have not been retrieved
 * are discarded.
 */
WASM_API_EXTERN void
wasmtime_async_driver_delete(wasmtime_async_driver_t *driver);

/**
 * \brief Get the I/O reactor file descriptor (Unix only).
 *
 * The host should monitor this fd for readability. When it becomes readable,
 * call \ref wasmtime_async_driver_poll_once.
 *
 * On Windows, returns -1. Use \ref wasmtime_async_driver_set_notify_socket
 * instead.
 *
 * \param driver The async driver.
 * \return The fd to monitor, or -1 on Windows.
 */
WASM_API_EXTERN int64_t
wasmtime_async_driver_io_fd(const wasmtime_async_driver_t *driver);

/**
 * \brief Set the notification socket (Windows only).
 *
 * The host creates a socket pair and passes the write end here. The driver
 * will send one byte to this socket when a task completes.
 *
 * Must be called before any tasks are spawned.
 *
 * \param driver The async driver.
 * \param socket_handle The write end of the notification socket pair.
 */
WASM_API_EXTERN void wasmtime_async_driver_set_notify_socket(
    wasmtime_async_driver_t *driver, uint64_t socket_handle);

/**
 * \brief Start the background event loop thread (Windows only).
 *
 * On Unix this is a no-op.
 *
 * \param driver The async driver.
 */
WASM_API_EXTERN void
wasmtime_async_driver_start_background(wasmtime_async_driver_t *driver);

/**
 * \brief Run one non-blocking iteration of the event loop (Unix only).
 *
 * This processes pending I/O events, fires expired timers, and advances
 * woken tasks.
 *
 * \param driver The async driver.
 * \return
 *   - Positive: next timer deadline in milliseconds. The host should call
 *     poll_once again after this duration if no fd event fires sooner.
 *   - 0: more work is available immediately (call again).
 *   - -1: no pending timers (wait for fd readability).
 *
 * On Windows, always returns -1.
 */
WASM_API_EXTERN int64_t
wasmtime_async_driver_poll_once(wasmtime_async_driver_t *driver);

/**
 * \brief Poll the status of a task.
 *
 * \param driver The async driver.
 * \param task_id The task ID returned by the spawning function.
 * \return One of the \ref wasmtime_async_task_status values.
 */
WASM_API_EXTERN int32_t
wasmtime_async_driver_task_poll(const wasmtime_async_driver_t *driver,
                                uint64_t task_id);

/**
 * \brief Retrieve the response body of a completed task.
 *
 * Returns NULL if the task is still pending or completed with an error.
 * The returned string is null-terminated and must be freed with
 * \ref wasmtime_async_driver_free_string.
 *
 * This consumes the result; calling again for the same task_id returns NULL.
 *
 * \param driver The async driver.
 * \param task_id The task ID.
 * \return The response string, or NULL.
 */
WASM_API_EXTERN char *
wasmtime_async_driver_task_get_response(wasmtime_async_driver_t *driver,
                                        uint64_t task_id);

/**
 * \brief Retrieve the error message of a failed task.
 *
 * Returns NULL if the task is still pending or completed successfully.
 * The returned string is null-terminated and must be freed with
 * \ref wasmtime_async_driver_free_string.
 *
 * This consumes the error; calling again for the same task_id returns NULL.
 *
 * \param driver The async driver.
 * \param task_id The task ID.
 * \return The error string, or NULL.
 */
WASM_API_EXTERN char *
wasmtime_async_driver_task_get_error(wasmtime_async_driver_t *driver,
                                     uint64_t task_id);

/**
 * \brief Free a string returned by task_get_response or task_get_error.
 *
 * \param ptr The string pointer to free. NULL is a safe no-op.
 */
WASM_API_EXTERN void wasmtime_async_driver_free_string(char *ptr);

#ifdef __cplusplus
} // extern "C"
#endif

#endif // WASMTIME_ASYNC_DRIVER_H
