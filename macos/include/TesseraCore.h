#ifndef TESSERA_CORE_H
#define TESSERA_CORE_H
#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>
typedef struct { uint8_t *ptr; size_t len; } TSBuffer;
void *ts_create(const uint8_t *path, size_t len, uint32_t threads);
void ts_cancel(const void *session);
void ts_release(void *session);
void ts_free(TSBuffer buffer);
TSBuffer ts_status(const void *session);
TSBuffer ts_layout(const void *session, uint32_t view, float width, float height, bool logical, bool force);
TSBuffer ts_tile_name(const void *session, uint32_t id);
TSBuffer ts_details(const void *session, uint32_t id);
TSBuffer ts_nav(const void *session, uint32_t id, bool logical);
TSBuffer ts_categories(const void *session, uint32_t id, bool logical);
TSBuffer ts_children(const void *session, uint32_t id, uint32_t offset, bool logical);
TSBuffer ts_refresh(const void *session, const uint8_t *path, size_t len);
TSBuffer ts_validate(const void *session, uint32_t id);
void *ts_watch_create(const uint8_t *path, size_t len, void (*callback)(const uint8_t *, size_t, uint32_t, void *), void *context);
void ts_watch_release(void *stream);
#endif
