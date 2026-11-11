/* PenguDroid host-only video ABI. No FFmpeg/Android layouts cross this boundary. */
#ifndef PENGUDROID_VIDEO_BRIDGE_H
#define PENGUDROID_VIDEO_BRIDGE_H
#include <stdint.h>
#define PD_VIDEO_ABI 1
#define PD_VIDEO_MAX_OBJECTS 4
#define PD_VIDEO_MAX_PLANES 4
/* Frames a consumer may hold at once. The daemon's auto-presenting session
 * keeps 4; its held session keeps up to 32 (24 unshown + 8 shown). */
#define PD_VIDEO_MAX_HELD 40
struct pd_video_decoder;
struct pd_video_frame;
struct pd_video_object { int32_t fd; uint32_t size; uint64_t modifier; };
struct pd_video_plane { uint32_t object_index, offset, stride; };
struct pd_video_frame_info {
    uint32_t width, height, fourcc, object_count, plane_count;
    int64_t pts_us;
    struct pd_video_object objects[PD_VIDEO_MAX_OBJECTS];
    struct pd_video_plane planes[PD_VIDEO_MAX_PLANES];
};
uint32_t pd_video_abi(void);
/* codec is h264/hevc/vp8/vp9/av1; device is a DRM render node. Hardware only:
 * VA-API when the driver has it, otherwise Vulkan Video. Never software. */
struct pd_video_decoder *pd_video_open(const char *codec, const char *device,
    const uint8_t *extra, uint32_t extra_size, char *error, uint32_t error_size);
void pd_video_close(struct pd_video_decoder *decoder);
/* 0 accepted, 1 drain frames before retry, negative error. NULL/0 sends EOS. */
int pd_video_send(struct pd_video_decoder *, const uint8_t *, uint32_t, int64_t,
    char *error, uint32_t error_size);
/* 0 frame, 1 needs input, 2 EOS, 3 all frame slots held, negative error. */
int pd_video_receive(struct pd_video_decoder *, struct pd_video_frame **,
    struct pd_video_frame_info *, char *error, uint32_t error_size);
void pd_video_frame_release(struct pd_video_frame *);
void pd_video_flush(struct pd_video_decoder *);
/* "vaapi" or "vulkan". Optional: absent from plugins built before it existed. */
const char *pd_video_backend(const struct pd_video_decoder *);
/* Explicit diagnostic readbacks; the playback/export path never calls these.
 * Luma: first sample of a YUV (VA-API) frame. RGBA: the whole of an RGBA
 * (Vulkan) frame, width * height * 4 tightly packed bytes. Each fails on the
 * other kind of frame. The RGBA one is optional, like pd_video_backend. */
int pd_video_test_luma(struct pd_video_frame *, uint32_t *sample,
    char *error, uint32_t error_size);
int pd_video_test_rgba(struct pd_video_frame *, uint8_t *pixels, uint32_t capacity,
    char *error, uint32_t error_size);
#endif
