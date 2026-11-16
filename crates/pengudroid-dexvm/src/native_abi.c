/* JNI native signatures use both integer and floating-point register banks.
 * libffi describes that ABI; a cast to a three-integer function cannot. */
#include <ffi.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
typedef struct { ffi_cif cif; size_t count; ffi_type **types; } pengu_cif;
static ffi_type *java_type(char type) {
    switch (type) {
    case 'V': return &ffi_type_void;
    case 'Z': return &ffi_type_uint8;
    case 'B': return &ffi_type_sint8;
    case 'C': return &ffi_type_uint16;
    case 'S': return &ffi_type_sint16;
    case 'I': return &ffi_type_sint32;
    case 'J': return &ffi_type_sint64;
    case 'F': return &ffi_type_float;
    case 'D': return &ffi_type_double;
    case 'L': return &ffi_type_pointer;
    default: return NULL;
    }
}
void *pengu_prepare_native(const char *shorty) {
    size_t len = strlen(shorty);
    if (!len || len > 256 || !java_type(shorty[0])) return NULL;
    pengu_cif *call = calloc(1, sizeof(*call));
    if (!call) return NULL;
    call->count = len + 1; /* hidden JNIEnv + jobject/jclass */
    call->types = calloc(call->count, sizeof(*call->types));
    if (!call->types) { free(call); return NULL; }
    call->types[0] = call->types[1] = &ffi_type_pointer;
    for (size_t i = 1; i < len; ++i) {
        call->types[i+1] = java_type(shorty[i]);
        if (!call->types[i+1] || shorty[i] == 'V') { free(call->types); free(call); return NULL; }
    }
    if (ffi_prep_cif(&call->cif, FFI_DEFAULT_ABI, call->count, java_type(shorty[0]), call->types) != FFI_OK) {
        free(call->types); free(call); return NULL;
    }
    return call;
}
void pengu_release_native(void *ptr) {
    pengu_cif *call = ptr;
    if (call) { free(call->types); free(call); }
}
void pengu_call_native(void *ptr, void *fn, uint64_t *cells, void **values, uint64_t *result) {
    pengu_cif *call = ptr;
    for (size_t i = 0; i < call->count; ++i) values[i] = &cells[i];
    *result = 0;
    ffi_call(&call->cif, FFI_FN(fn), result, values);
}

/* JNI variadic and va_list entry points must use C's default promotions.
 * Forward a real jvalue array to Rust rather than guessing three registers. */
#include <stdarg.h>
extern int pengu_jni_shorty(void *method, char *out, size_t size);
extern uint64_t pengu_jni_dispatch(void *env, void *object, void *method, int mode, const uint64_t *values);
static uint64_t dispatch_va(void *env, void *object, void *method, int mode, va_list source) {
    char shorty[257];
    uint64_t values[256] = {0};
    int count = pengu_jni_shorty(method, shorty, sizeof shorty);
    if (count < 1 || count > 256) return 0;
    va_list args; va_copy(args, source);
    for (int i = 1; i < count; ++i) {
        switch (shorty[i]) {
        case 'Z': case 'B': case 'C': case 'S': case 'I': values[i-1] = (uint64_t)va_arg(args, int); break;
        case 'J': values[i-1] = (uint64_t)va_arg(args, int64_t); break;
        case 'F': { float value = (float)va_arg(args, double); memcpy(&values[i-1], &value, sizeof value); break; }
        case 'D': { double value = va_arg(args, double); memcpy(&values[i-1], &value, sizeof value); break; }
        case 'L': values[i-1] = (uintptr_t)va_arg(args, void *); break;
        default: va_end(args); return 0;
        }
    }
    va_end(args);
    return pengu_jni_dispatch(env, object, method, mode, values);
}
static float result_Float(uint64_t bits) { float value; uint32_t word = bits; memcpy(&value, &word, sizeof value); return value; }
static double result_Double(uint64_t bits) { double value; memcpy(&value, &bits, sizeof value); return value; }
#define result_Object(bits) ((void *)(uintptr_t)(bits))
#define result_Boolean(bits) ((uint8_t)(bits))
#define result_Byte(bits) ((int8_t)(bits))
#define result_Char(bits) ((uint16_t)(bits))
#define result_Short(bits) ((int16_t)(bits))
#define result_Int(bits) ((int32_t)(bits))
#define result_Long(bits) ((int64_t)(bits))
#define result_Void(bits) ((void)(bits))
#define JNI_CALLS(Name, Type) \
Type pengu_call_##Name(void *env, void *obj, void *method, ...) { \
    va_list args; va_start(args, method); uint64_t value = dispatch_va(env,obj,method,0,args); va_end(args); return result_##Name(value); } \
Type pengu_call_##Name##_v(void *env, void *obj, void *method, va_list args) { return result_##Name(dispatch_va(env,obj,method,0,args)); } \
Type pengu_call_##Name##_a(void *env, void *obj, void *method, const uint64_t *args) { return result_##Name(pengu_jni_dispatch(env,obj,method,0,args)); } \
Type pengu_nonvirtual_##Name(void *env, void *obj, void *cls, void *method, ...) { \
    (void)cls; va_list args; va_start(args, method); uint64_t value = dispatch_va(env,obj,method,2,args); va_end(args); return result_##Name(value); } \
Type pengu_nonvirtual_##Name##_v(void *env, void *obj, void *cls, void *method, va_list args) { (void)cls; return result_##Name(dispatch_va(env,obj,method,2,args)); } \
Type pengu_nonvirtual_##Name##_a(void *env, void *obj, void *cls, void *method, const uint64_t *args) { (void)cls; return result_##Name(pengu_jni_dispatch(env,obj,method,2,args)); } \
Type pengu_static_##Name(void *env, void *cls, void *method, ...) { \
    va_list args; va_start(args, method); uint64_t value = dispatch_va(env,cls,method,1,args); va_end(args); return result_##Name(value); } \
Type pengu_static_##Name##_v(void *env, void *cls, void *method, va_list args) { return result_##Name(dispatch_va(env,cls,method,1,args)); } \
Type pengu_static_##Name##_a(void *env, void *cls, void *method, const uint64_t *args) { return result_##Name(pengu_jni_dispatch(env,cls,method,1,args)); }
JNI_CALLS(Object, void *)
JNI_CALLS(Boolean, uint8_t)
JNI_CALLS(Byte, int8_t)
JNI_CALLS(Char, uint16_t)
JNI_CALLS(Short, int16_t)
JNI_CALLS(Int, int32_t)
JNI_CALLS(Long, int64_t)
JNI_CALLS(Float, float)
JNI_CALLS(Double, double)
JNI_CALLS(Void, void)
void *pengu_new_object(void *env, void *cls, void *method, ...) {
    va_list args; va_start(args, method); uint64_t value = dispatch_va(env,cls,method,3,args); va_end(args); return result_Object(value);
}
void *pengu_new_object_v(void *env, void *cls, void *method, va_list args) { return result_Object(dispatch_va(env,cls,method,3,args)); }
#define WIRE(Name, Offset) \
    table[34+Offset*3]=(uintptr_t)pengu_call_##Name; table[35+Offset*3]=(uintptr_t)pengu_call_##Name##_v; table[36+Offset*3]=(uintptr_t)pengu_call_##Name##_a; \
    table[64+Offset*3]=(uintptr_t)pengu_nonvirtual_##Name; table[65+Offset*3]=(uintptr_t)pengu_nonvirtual_##Name##_v; table[66+Offset*3]=(uintptr_t)pengu_nonvirtual_##Name##_a; \
    table[114+Offset*3]=(uintptr_t)pengu_static_##Name; table[115+Offset*3]=(uintptr_t)pengu_static_##Name##_v; table[116+Offset*3]=(uintptr_t)pengu_static_##Name##_a;
void pengu_install_jni_calls(uintptr_t *table) {
    table[28]=(uintptr_t)pengu_new_object; table[29]=(uintptr_t)pengu_new_object_v;
    WIRE(Object,0) WIRE(Boolean,1) WIRE(Byte,2) WIRE(Char,3) WIRE(Short,4)
    WIRE(Int,5) WIRE(Long,6) WIRE(Float,7) WIRE(Double,8) WIRE(Void,9)
}

/* Exercise genuine C ellipsis, va_list and jvalue callers in Rust regression tests. */
static double test_call_v(void *env, void *object, void *method, ...) {
    uintptr_t *table = *(uintptr_t **)env;
    va_list args; va_start(args, method);
    double value = ((double (*)(void *,void *,void *,va_list))table[59])(env,object,method,args);
    va_end(args); return value;
}
int pengu_test_jni_calls(void *env, void *object, void *method, double *results) {
    uintptr_t *table = *(uintptr_t **)env;
    uint64_t args[6] = {(uint64_t)-7,0,0,10000000000ULL,9,0x1234};
    float f = 1.25f; double d = 2.5;
    memcpy(&args[1], &f, sizeof f); memcpy(&args[2], &d, sizeof d);
    results[0]=((double (*)(void *,void *,void *,...))table[58])(env,object,method,-7,1.25,2.5,(int64_t)10000000000LL,9,(void *)0x1234);
    results[1]=test_call_v(env,object,method,-7,1.25,2.5,(int64_t)10000000000LL,9,(void *)0x1234);
    results[2]=((double (*)(void *,void *,void *,const uint64_t *))table[60])(env,object,method,args);
    return 0;
}
