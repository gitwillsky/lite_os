#define _GNU_SOURCE
#include <quickjs.h>

#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

typedef int (*LiteUiCommit)(void *, const uint8_t *, size_t, uint32_t);

typedef struct {
    JSRuntime *runtime;
    JSContext *context;
    void *opaque;
    LiteUiCommit commit;
    uint64_t deadline_ns;
    JSValue event_callback;
} LiteJs;

static int copy_exception(LiteJs *engine, uint8_t *output, size_t capacity);

static uint64_t monotonic_ns(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
        return UINT64_MAX;
    }
    return (uint64_t)value.tv_sec * 1000000000u + (uint64_t)value.tv_nsec;
}

static int interrupt(JSRuntime *runtime, void *opaque) {
    (void)runtime;
    const LiteJs *engine = opaque;
    return monotonic_ns() >= engine->deadline_ns;
}

static void set_deadline(LiteJs *engine, uint32_t milliseconds) {
    const uint64_t now = monotonic_ns();
    const uint64_t duration = (uint64_t)milliseconds * 1000000u;
    engine->deadline_ns = now > UINT64_MAX - duration ? UINT64_MAX : now + duration;
}

static JSValue js_commit(
    JSContext *context,
    JSValueConst this_value,
    int argument_count,
    JSValueConst *arguments
) {
    (void)this_value;
    if (argument_count != 2) {
        return JS_ThrowTypeError(context, "commit requires bytes and operation count");
    }
    uint32_t operations;
    if (JS_ToUint32(context, &operations, arguments[1]) < 0) {
        return JS_EXCEPTION;
    }
    size_t offset;
    size_t length;
    size_t element_size;
    JSValue buffer = JS_GetTypedArrayBuffer(
        context,
        arguments[0],
        &offset,
        &length,
        &element_size
    );
    if (JS_IsException(buffer)) {
        return buffer;
    }
    size_t buffer_length;
    uint8_t *bytes = JS_GetArrayBuffer(context, &buffer_length, buffer);
    if (bytes == NULL || element_size != 1 || offset > buffer_length || length > buffer_length - offset) {
        JS_FreeValue(context, buffer);
        return JS_ThrowTypeError(context, "commit bytes must be a Uint8Array");
    }
    LiteJs *engine = JS_GetContextOpaque(context);
    const int result = engine->commit(engine->opaque, bytes + offset, length, operations);
    JS_FreeValue(context, buffer);
    return JS_NewInt32(context, result);
}

static JSValue js_on_event(
    JSContext *context,
    JSValueConst this_value,
    int argument_count,
    JSValueConst *arguments
) {
    (void)this_value;
    if (argument_count != 1 || !JS_IsFunction(context, arguments[0])) {
        return JS_ThrowTypeError(context, "onEvent requires one function");
    }
    LiteJs *engine = JS_GetContextOpaque(context);
    JS_FreeValue(context, engine->event_callback);
    engine->event_callback = JS_DupValue(context, arguments[0]);
    return JS_UNDEFINED;
}

static int install_host_ops(LiteJs *engine) {
    JSValue global = JS_GetGlobalObject(engine->context);
    JSValue liteui = JS_NewObject(engine->context);
    if (JS_IsException(global) || JS_IsException(liteui)) {
        JS_FreeValue(engine->context, liteui);
        JS_FreeValue(engine->context, global);
        return -1;
    }
    JSValue commit = JS_NewCFunction(engine->context, js_commit, "commit", 2);
    JSValue on_event = JS_NewCFunction(engine->context, js_on_event, "onEvent", 1);
    if (JS_IsException(commit) || JS_IsException(on_event)
        || JS_SetPropertyStr(engine->context, liteui, "commit", commit) < 0
        || JS_SetPropertyStr(engine->context, liteui, "onEvent", on_event) < 0
        || JS_SetPropertyStr(engine->context, global, "LiteUI", liteui) < 0) {
        JS_FreeValue(engine->context, global);
        return -1;
    }
    JS_FreeValue(engine->context, global);
    return 0;
}

LiteJs *litejs_create(
    size_t heap_limit,
    size_t stack_limit,
    void *opaque,
    LiteUiCommit commit
) {
    if (heap_limit == 0 || stack_limit == 0 || commit == NULL) {
        return NULL;
    }
    LiteJs *engine = calloc(1, sizeof(*engine));
    if (engine == NULL) {
        return NULL;
    }
    engine->opaque = opaque;
    engine->commit = commit;
    engine->event_callback = JS_UNDEFINED;
    engine->runtime = JS_NewRuntime();
    if (engine->runtime == NULL) {
        free(engine);
        return NULL;
    }
    JS_SetMemoryLimit(engine->runtime, heap_limit);
    JS_SetMaxStackSize(engine->runtime, stack_limit);
    JS_SetCanBlock(engine->runtime, 0);
    JS_SetInterruptHandler(engine->runtime, interrupt, engine);
    engine->context = JS_NewContext(engine->runtime);
    if (engine->context == NULL || install_host_ops(engine) != 0) {
        if (engine->context != NULL) {
            JS_FreeContext(engine->context);
        }
        JS_FreeRuntime(engine->runtime);
        free(engine);
        return NULL;
    }
    JS_SetContextOpaque(engine->context, engine);
    return engine;
}

void litejs_destroy(LiteJs *engine) {
    if (engine == NULL) {
        return;
    }
    JS_FreeValue(engine->context, engine->event_callback);
    JS_FreeContext(engine->context);
    JS_FreeRuntime(engine->runtime);
    free(engine);
}

int litejs_dispatch_click(
    LiteJs *engine,
    uint16_t node,
    uint16_t generation,
    uint32_t deadline_ms,
    uint8_t *error,
    size_t error_capacity
) {
    if (engine == NULL || node == 0 || generation == 0 || error == NULL
        || JS_IsUndefined(engine->event_callback)) {
        return -1;
    }
    set_deadline(engine, deadline_ms);
    JSValue arguments[2] = {
        JS_NewUint32(engine->context, node),
        JS_NewUint32(engine->context, generation),
    };
    JSValue result = JS_Call(
        engine->context,
        engine->event_callback,
        JS_UNDEFINED,
        2,
        arguments
    );
    JS_FreeValue(engine->context, arguments[0]);
    JS_FreeValue(engine->context, arguments[1]);
    if (JS_IsException(result)) {
        return copy_exception(engine, error, error_capacity);
    }
    JS_FreeValue(engine->context, result);
    return 0;
}

static int copy_exception(LiteJs *engine, uint8_t *output, size_t capacity) {
    if (capacity == 0) {
        return -1;
    }
    JSValue exception = JS_GetException(engine->context);
    JSValue stack = JS_GetPropertyStr(engine->context, exception, "stack");
    const JSValueConst message = JS_IsString(stack) ? stack : exception;
    size_t length;
    const char *text = JS_ToCStringLen(engine->context, &length, message);
    if (text != NULL) {
        const size_t copied = length < capacity - 1 ? length : capacity - 1;
        memcpy(output, text, copied);
        output[copied] = 0;
        JS_FreeCString(engine->context, text);
    } else {
        output[0] = 0;
    }
    JS_FreeValue(engine->context, stack);
    JS_FreeValue(engine->context, exception);
    return -1;
}

int litejs_compile_module(
    LiteJs *engine,
    const uint8_t *source,
    size_t source_length,
    const char *filename,
    uint32_t deadline_ms,
    uint8_t **bytecode,
    size_t *bytecode_length,
    uint8_t *error,
    size_t error_capacity
) {
    if (engine == NULL || source == NULL || filename == NULL
        || bytecode == NULL || bytecode_length == NULL || error == NULL) {
        return -1;
    }
    *bytecode = NULL;
    *bytecode_length = 0;
    set_deadline(engine, deadline_ms);
    JSValue module = JS_Eval(
        engine->context,
        (const char *)source,
        source_length,
        filename,
        JS_EVAL_TYPE_MODULE | JS_EVAL_FLAG_STRICT | JS_EVAL_FLAG_COMPILE_ONLY
    );
    if (JS_IsException(module)) {
        return copy_exception(engine, error, error_capacity);
    }
    *bytecode = JS_WriteObject(
        engine->context,
        bytecode_length,
        module,
        JS_WRITE_OBJ_BYTECODE
    );
    if (*bytecode == NULL || JS_ResolveModule(engine->context, module) < 0) {
        if (*bytecode != NULL) {
            js_free(engine->context, *bytecode);
            *bytecode = NULL;
            *bytecode_length = 0;
        }
        JS_FreeValue(engine->context, module);
        return copy_exception(engine, error, error_capacity);
    }
    JSValue result = JS_EvalFunction(engine->context, module);
    if (JS_IsException(result)) {
        js_free(engine->context, *bytecode);
        *bytecode = NULL;
        *bytecode_length = 0;
        return copy_exception(engine, error, error_capacity);
    }
    JS_FreeValue(engine->context, result);
    return 0;
}

int litejs_eval_bytecode(
    LiteJs *engine,
    const uint8_t *bytecode,
    size_t bytecode_length,
    uint32_t deadline_ms,
    uint8_t *error,
    size_t error_capacity
) {
    if (engine == NULL || bytecode == NULL || error == NULL) {
        return -1;
    }
    set_deadline(engine, deadline_ms);
    JSValue module = JS_ReadObject(
        engine->context,
        bytecode,
        bytecode_length,
        JS_READ_OBJ_BYTECODE
    );
    if (JS_IsException(module) || JS_ResolveModule(engine->context, module) < 0) {
        JS_FreeValue(engine->context, module);
        return copy_exception(engine, error, error_capacity);
    }
    JSValue result = JS_EvalFunction(engine->context, module);
    if (JS_IsException(result)) {
        return copy_exception(engine, error, error_capacity);
    }
    JS_FreeValue(engine->context, result);
    return 0;
}

void litejs_free_buffer(LiteJs *engine, uint8_t *buffer) {
    if (engine != NULL && buffer != NULL) {
        js_free(engine->context, buffer);
    }
}

int litejs_execute_jobs(
    LiteJs *engine,
    uint32_t budget,
    uint32_t deadline_ms,
    uint8_t *error,
    size_t error_capacity
) {
    if (engine == NULL || error == NULL) {
        return -1;
    }
    set_deadline(engine, deadline_ms);
    for (uint32_t index = 0; index < budget; ++index) {
        JSContext *context = NULL;
        const int result = JS_ExecutePendingJob(engine->runtime, &context);
        if (result == 0) {
            return 0;
        }
        if (result < 0) {
            return copy_exception(engine, error, error_capacity);
        }
    }
    if (error_capacity != 0) {
        const char message[] = "pending job budget exceeded";
        const size_t copied = sizeof(message) < error_capacity ? sizeof(message) : error_capacity;
        memcpy(error, message, copied);
        error[copied - 1] = 0;
    }
    return -1;
}
