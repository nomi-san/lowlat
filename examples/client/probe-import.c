// Does the toolkit's GL context import a descriptor the compute runtime
// exported, and does a picture written through the runtime come out of a
// texture filled from it? A probe, not a product: it opens the runtime by
// hand, writes a known picture into an exportable allocation, imports the
// descriptor as a GL buffer on the toolkit's context, fills textures from
// that buffer the way the demo would, and reads them back.
//
//   make -C examples/client probe-import && DISPLAY=:0 ./examples/client/probe-import
//
// Prints one line per step and "IMPORT OK" or "IMPORT FAILED" at the end.

#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "matoya.h"
#include "glcorearb.h"

// The compute runtime, by hand: the few entry points the probe needs.

typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef unsigned long long CUdeviceptr;
typedef unsigned long long CUmemGenericAllocationHandle;

typedef struct {
	unsigned type;
	int id;
} CUmemLocation;

typedef struct {
	unsigned type;
	unsigned requestedHandleTypes;
	CUmemLocation location;
	void *win32HandleMetaData;
	struct {
		unsigned char compressionType;
		unsigned char gpuDirectRDMACapable;
		unsigned short usage;
		unsigned char reserved[4];
	} allocFlags;
} CUmemAllocationProp;

typedef struct {
	CUmemLocation location;
	unsigned flags;
} CUmemAccessDesc;

static CUresult (*cuInit)(unsigned);
static CUresult (*cuDeviceGet)(CUdevice *, int);
static CUresult (*cuDevicePrimaryCtxRetain)(CUcontext *, CUdevice);
static CUresult (*cuCtxPushCurrent)(CUcontext);
static CUresult (*cuMemGetAllocationGranularity)(size_t *, const CUmemAllocationProp *, unsigned);
static CUresult (*cuMemCreate)(CUmemGenericAllocationHandle *, size_t, const CUmemAllocationProp *,
	unsigned long long);
static CUresult (*cuMemAddressReserve)(CUdeviceptr *, size_t, size_t, CUdeviceptr, unsigned long long);
static CUresult (*cuMemMap)(CUdeviceptr, size_t, size_t, CUmemGenericAllocationHandle, unsigned long long);
static CUresult (*cuMemSetAccess)(CUdeviceptr, size_t, const CUmemAccessDesc *, size_t);
static CUresult (*cuMemExportToShareableHandle)(void *, CUmemGenericAllocationHandle, unsigned,
	unsigned long long);
static CUresult (*cuMemcpyHtoD)(CUdeviceptr, const void *, size_t);
static CUresult (*cuCtxSynchronize)(void);

#define CU_SYM(handle, name, symbol) \
	do { \
		*(void **) &name = dlsym(handle, symbol); \
		if (!name) { \
			fprintf(stderr, "no %s in the runtime\n", symbol); \
			return false; \
		} \
	} while (0)

static bool load_cuda(void)
{
	void *so = dlopen("libcuda.so.1", RTLD_NOW);
	if (!so) {
		fprintf(stderr, "libcuda.so.1 did not open: %s\n", dlerror());
		return false;
	}
	CU_SYM(so, cuInit, "cuInit");
	CU_SYM(so, cuDeviceGet, "cuDeviceGet");
	CU_SYM(so, cuDevicePrimaryCtxRetain, "cuDevicePrimaryCtxRetain");
	CU_SYM(so, cuCtxPushCurrent, "cuCtxPushCurrent_v2");
	CU_SYM(so, cuMemGetAllocationGranularity, "cuMemGetAllocationGranularity");
	CU_SYM(so, cuMemCreate, "cuMemCreate");
	CU_SYM(so, cuMemAddressReserve, "cuMemAddressReserve");
	CU_SYM(so, cuMemMap, "cuMemMap");
	CU_SYM(so, cuMemSetAccess, "cuMemSetAccess");
	CU_SYM(so, cuMemExportToShareableHandle, "cuMemExportToShareableHandle");
	CU_SYM(so, cuMemcpyHtoD, "cuMemcpyHtoD_v2");
	CU_SYM(so, cuCtxSynchronize, "cuCtxSynchronize");
	return true;
}

#define CU_CHECK(call) \
	do { \
		CUresult r_ = (call); \
		if (r_ != 0) { \
			fprintf(stderr, "%s returned %d\n", #call, r_); \
			return false; \
		} \
	} while (0)

// The GL side the toolkit's own loader does not resolve.

typedef void (APIENTRYP PFNGLCREATEMEMORYOBJECTSEXTPROC)(GLsizei n, GLuint *memoryObjects);
typedef void (APIENTRYP PFNGLDELETEMEMORYOBJECTSEXTPROC)(GLsizei n, const GLuint *memoryObjects);
typedef void (APIENTRYP PFNGLIMPORTMEMORYFDEXTPROC)(GLuint memory, GLuint64 size, GLenum handleType, GLint fd);
typedef void (APIENTRYP PFNGLBUFFERSTORAGEMEMEXTPROC)(GLenum target, GLsizeiptr size, GLuint memory, GLuint64 offset);

#define GL_HANDLE_TYPE_OPAQUE_FD_EXT 0x9586

static PFNGLGETSTRINGPROC glGetString;
static PFNGLGETERRORPROC glGetError;
static PFNGLGENBUFFERSPROC glGenBuffers;
static PFNGLBINDBUFFERPROC glBindBuffer;
static PFNGLGETBUFFERSUBDATAPROC glGetBufferSubData;
static PFNGLGENTEXTURESPROC glGenTextures;
static PFNGLBINDTEXTUREPROC glBindTexture;
static PFNGLTEXIMAGE2DPROC glTexImage2D;
static PFNGLTEXSUBIMAGE2DPROC glTexSubImage2D;
static PFNGLGETTEXIMAGEPROC glGetTexImage;
static PFNGLPIXELSTOREIPROC glPixelStorei;
static PFNGLFINISHPROC glFinish;
static PFNGLCREATEMEMORYOBJECTSEXTPROC glCreateMemoryObjectsEXT;
static PFNGLDELETEMEMORYOBJECTSEXTPROC glDeleteMemoryObjectsEXT;
static PFNGLIMPORTMEMORYFDEXTPROC glImportMemoryFdEXT;
static PFNGLBUFFERSTORAGEMEMEXTPROC glBufferStorageMemEXT;

#define GL_SYM(name) \
	do { \
		*(void **) &name = MTY_GLGetProcAddress(#name); \
		if (!name) { \
			fprintf(stderr, "no %s on this context\n", #name); \
			return false; \
		} \
	} while (0)

static bool load_gl(void)
{
	GL_SYM(glGetString);
	GL_SYM(glGetError);
	GL_SYM(glGenBuffers);
	GL_SYM(glBindBuffer);
	GL_SYM(glGetBufferSubData);
	GL_SYM(glGenTextures);
	GL_SYM(glBindTexture);
	GL_SYM(glTexImage2D);
	GL_SYM(glTexSubImage2D);
	GL_SYM(glGetTexImage);
	GL_SYM(glPixelStorei);
	GL_SYM(glFinish);
	GL_SYM(glCreateMemoryObjectsEXT);
	GL_SYM(glDeleteMemoryObjectsEXT);
	GL_SYM(glImportMemoryFdEXT);
	GL_SYM(glBufferStorageMemEXT);
	return true;
}

static bool gl_clean(const char *step)
{
	GLenum e = glGetError();
	if (e != GL_NO_ERROR) {
		fprintf(stderr, "%s: GL error 0x%x\n", step, e);
		return false;
	}
	printf("%s: ok\n", step);
	return true;
}

// The picture: a luma plane and an interleaved chroma plane at one pitch,
// the layout the client's slots use, with a different function per plane
// so a wrong offset shows as a wrong plane rather than a near miss.

#define WIDTH 2560
#define HEIGHT 1440
#define PITCH WIDTH
#define LUMA_BYTES ((size_t) PITCH * HEIGHT)
#define CHROMA_BYTES ((size_t) PITCH * (HEIGHT / 2))
#define PICTURE_BYTES (LUMA_BYTES + CHROMA_BYTES)

static uint8_t *picture;

static void fill_picture(void)
{
	picture = malloc(PICTURE_BYTES);
	for (uint32_t y = 0; y < HEIGHT; y++)
		for (uint32_t x = 0; x < WIDTH; x++)
			picture[(size_t) y * PITCH + x] = (uint8_t) (x + y);
	for (uint32_t y = 0; y < HEIGHT / 2; y++)
		for (uint32_t x = 0; x < WIDTH; x++)
			picture[LUMA_BYTES + (size_t) y * PITCH + x] = (uint8_t) (x ^ (y * 3));
}

static size_t first_difference(const uint8_t *a, const uint8_t *b, size_t n)
{
	for (size_t i = 0; i < n; i++)
		if (a[i] != b[i])
			return i;
	return n;
}

static double now_ms(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (double) ts.tv_sec * 1000.0 + (double) ts.tv_nsec / 1e6;
}

static int exported_fd = -1;
static size_t exported_size;

static bool export_picture(void)
{
	CU_CHECK(cuInit(0));
	CUdevice device = 0;
	CU_CHECK(cuDeviceGet(&device, 0));
	CUcontext context = NULL;
	CU_CHECK(cuDevicePrimaryCtxRetain(&context, device));
	CU_CHECK(cuCtxPushCurrent(context));

	CUmemAllocationProp prop = {0};
	prop.type = 1; // pinned
	prop.requestedHandleTypes = 1; // a POSIX descriptor
	prop.location.type = 1; // a device
	prop.location.id = device;
	size_t granule = 0;
	CU_CHECK(cuMemGetAllocationGranularity(&granule, &prop, 0));
	exported_size = (PICTURE_BYTES + granule - 1) / granule * granule;
	printf("allocation: %zu bytes of picture in %zu (granule %zu)\n", PICTURE_BYTES, exported_size, granule);

	CUmemGenericAllocationHandle handle = 0;
	CU_CHECK(cuMemCreate(&handle, exported_size, &prop, 0));
	CUdeviceptr ptr = 0;
	CU_CHECK(cuMemAddressReserve(&ptr, exported_size, 0, 0, 0));
	CU_CHECK(cuMemMap(ptr, exported_size, 0, handle, 0));
	CUmemAccessDesc access = {0};
	access.location = prop.location;
	access.flags = 3; // read and write
	CU_CHECK(cuMemSetAccess(ptr, exported_size, &access, 1));
	CU_CHECK(cuMemExportToShareableHandle(&exported_fd, handle, 1, 0));
	printf("exported: descriptor %d\n", exported_fd);

	CU_CHECK(cuMemcpyHtoD(ptr, picture, PICTURE_BYTES));
	CU_CHECK(cuCtxSynchronize());
	printf("written: the picture is on the device\n");
	return true;
}

static bool import_and_check(void)
{
	if (!load_gl())
		return false;
	printf("context: %s, %s\n", (const char *) glGetString(GL_RENDERER), (const char *) glGetString(GL_VERSION));

	// The import takes ownership of the descriptor it is given, so it is
	// given a duplicate, as the demo will be.
	GLuint memory = 0;
	glCreateMemoryObjectsEXT(1, &memory);
	if (!gl_clean("create memory object"))
		return false;
	int dup_fd = dup(exported_fd);
	glImportMemoryFdEXT(memory, exported_size, GL_HANDLE_TYPE_OPAQUE_FD_EXT, dup_fd);
	if (!gl_clean("import the descriptor"))
		return false;

	GLuint buffer = 0;
	glGenBuffers(1, &buffer);
	glBindBuffer(GL_PIXEL_UNPACK_BUFFER, buffer);
	glBufferStorageMemEXT(GL_PIXEL_UNPACK_BUFFER, (GLsizeiptr) exported_size, memory, 0);
	if (!gl_clean("buffer over the memory"))
		return false;

	// First proof: the buffer's bytes are the runtime's.
	uint8_t *back = malloc(PICTURE_BYTES);
	memset(back, 0, PICTURE_BYTES);
	glGetBufferSubData(GL_PIXEL_UNPACK_BUFFER, 0, (GLsizeiptr) PICTURE_BYTES, back);
	if (!gl_clean("read the buffer"))
		return false;
	size_t diff = first_difference(picture, back, PICTURE_BYTES);
	printf("buffer read-back: %s (first difference at %zu of %zu)\n",
		diff == PICTURE_BYTES ? "matches" : "DIFFERS", diff, PICTURE_BYTES);
	if (diff != PICTURE_BYTES)
		return false;

	// Second proof: textures filled from the buffer, the demo's path. The
	// luma plane as R8 at the picture's pitch, the chroma plane as RG8
	// from its offset.
	GLuint textures[2] = {0};
	glGenTextures(2, textures);
	glBindTexture(GL_TEXTURE_2D, textures[0]);
	glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, WIDTH, HEIGHT, 0, GL_RED, GL_UNSIGNED_BYTE, NULL);
	glBindTexture(GL_TEXTURE_2D, textures[1]);
	glTexImage2D(GL_TEXTURE_2D, 0, GL_RG8, WIDTH / 2, HEIGHT / 2, 0, GL_RG, GL_UNSIGNED_BYTE, NULL);
	if (!gl_clean("textures"))
		return false;

	double best = 1e9, total = 0;
	const int rounds = 200;
	for (int n = 0; n < rounds; n++) {
		double t0 = now_ms();
		glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
		glBindTexture(GL_TEXTURE_2D, textures[0]);
		glPixelStorei(GL_UNPACK_ROW_LENGTH, PITCH);
		glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, WIDTH, HEIGHT, GL_RED, GL_UNSIGNED_BYTE, (const void *) 0);
		glBindTexture(GL_TEXTURE_2D, textures[1]);
		glPixelStorei(GL_UNPACK_ROW_LENGTH, PITCH / 2);
		glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, WIDTH / 2, HEIGHT / 2, GL_RG, GL_UNSIGNED_BYTE,
			(const void *) (uintptr_t) LUMA_BYTES);
		glFinish();
		double dt = now_ms() - t0;
		total += dt;
		if (dt < best)
			best = dt;
	}
	if (!gl_clean("fill the textures from the buffer"))
		return false;
	printf("texture fill from the buffer, %ux%u two planes: best %.3f ms, mean %.3f ms (with a finish)\n",
		WIDTH, HEIGHT, best, total / rounds);

	glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
	glPixelStorei(GL_PACK_ALIGNMENT, 1);
	memset(back, 0, PICTURE_BYTES);
	glBindTexture(GL_TEXTURE_2D, textures[0]);
	glGetTexImage(GL_TEXTURE_2D, 0, GL_RED, GL_UNSIGNED_BYTE, back);
	glBindTexture(GL_TEXTURE_2D, textures[1]);
	glGetTexImage(GL_TEXTURE_2D, 0, GL_RG, GL_UNSIGNED_BYTE, back + LUMA_BYTES);
	if (!gl_clean("read the textures"))
		return false;
	diff = first_difference(picture, back, PICTURE_BYTES);
	printf("texture read-back: %s (first difference at %zu of %zu)\n",
		diff == PICTURE_BYTES ? "matches" : "DIFFERS", diff, PICTURE_BYTES);
	free(back);
	return diff == PICTURE_BYTES;
}

static bool app_func(void *opaque)
{
	return false;
}

static void event_func(const MTY_Event *evt, void *opaque)
{
}

int main(void)
{
	fill_picture();
	if (!load_cuda() || !export_picture()) {
		printf("IMPORT FAILED (the runtime side)\n");
		return 1;
	}

	MTY_App *app = MTY_AppCreate(0, app_func, event_func, NULL);
	MTY_Frame frame = {.size = {.w = 320, .h = 240}};
	MTY_Window window = MTY_WindowCreate(app, "probe-import", &frame, 0);
	if (window < 0 || !MTY_WindowSetGFX(app, window, MTY_GFX_GL, false)) {
		printf("IMPORT FAILED (no GL context)\n");
		return 1;
	}

	bool ok = import_and_check();
	printf(ok ? "IMPORT OK\n" : "IMPORT FAILED\n");
	MTY_WindowSetGFX(app, window, MTY_GFX_NONE, false);
	MTY_AppDestroy(&app);
	return ok ? 0 : 1;
}
