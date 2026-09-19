// This Source Code Form is subject to the terms of the MIT License.
// If a copy of the MIT License was not distributed with this file,
// You can obtain one at https://spdx.org/licenses/MIT.html.

#pragma once

#if defined(MTY_GL_ES)
	#define GL_SHADER_VERSION "#version 100\n"
#else
	#define GL_SHADER_VERSION "#version 110\n"
#endif

#if defined(MTY_GL_EXTERNAL)
	#define GL_GLEXT_PROTOTYPES
#endif

#include "glcorearb.h"

// The external-memory extensions (GL_EXT_memory_object, GL_EXT_memory_object_fd), which the
// core header does not carry: what a hardware frame is imported through.
#define GL_HANDLE_TYPE_OPAQUE_FD_EXT 0x9586

typedef void (APIENTRYP PFNGLCREATEMEMORYOBJECTSEXTPROC)(GLsizei n, GLuint *memoryObjects);
typedef void (APIENTRYP PFNGLDELETEMEMORYOBJECTSEXTPROC)(GLsizei n, const GLuint *memoryObjects);
typedef void (APIENTRYP PFNGLIMPORTMEMORYFDEXTPROC)(GLuint memory, GLuint64 size, GLenum handleType, GLint fd);
typedef void (APIENTRYP PFNGLBUFFERSTORAGEMEMEXTPROC)(GLenum target, GLsizeiptr size, GLuint memory, GLuint64 offset);
