#ifndef FOZZY_MEGASERVER_H
#define FOZZY_MEGASERVER_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef int32_t (*fz_callback_i32_v0)(int32_t arg);
int32_t fz_host_init(void);
int32_t fz_host_shutdown(void);
int32_t fz_host_cleanup(void);
int32_t fz_host_last_error_code(void);
int32_t fz_host_last_error_class(void);
const char* fz_host_last_error_message(void);
int32_t fz_host_register_callback_i32(int32_t slot, fz_callback_i32_v0 cb);
int32_t fz_host_invoke_callback_i32(int32_t slot, int32_t arg);

int32_t megaserver_fzy_schema_version(void);
int32_t megaserver_fzy_plan_manifest(void);
int32_t megaserver_fzy_dispatch_control(void);

#ifdef __cplusplus
}
#endif

#endif
