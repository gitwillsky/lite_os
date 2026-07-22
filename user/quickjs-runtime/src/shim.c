#include "quickjs.h"

extern int lite_qjs_host_call(
    void *opaque,
    const char *operation,
    size_t operation_len,
    const char *payload,
    size_t payload_len,
    const char **response,
    size_t *response_len
);

static JSValue lite_qjs_native(
    JSContext *context,
    JSValueConst this_value,
    int argc,
    JSValueConst *argv
) {
    (void)this_value;
    if (argc != 2) {
        return JS_ThrowTypeError(context, "__liteNative requires operation and payload");
    }
    size_t operation_len = 0;
    size_t payload_len = 0;
    const char *operation = JS_ToCStringLen(context, &operation_len, argv[0]);
    const char *payload = JS_ToCStringLen(context, &payload_len, argv[1]);
    if (operation == NULL || payload == NULL) {
        if (operation != NULL) {
            JS_FreeCString(context, operation);
        }
        if (payload != NULL) {
            JS_FreeCString(context, payload);
        }
        return JS_EXCEPTION;
    }
    const char *response = NULL;
    size_t response_len = 0;
    int status = lite_qjs_host_call(
        JS_GetContextOpaque(context),
        operation,
        operation_len,
        payload,
        payload_len,
        &response,
        &response_len
    );
    JS_FreeCString(context, operation);
    JS_FreeCString(context, payload);
    if (status != 0) {
        return JS_ThrowInternalError(context, "%.*s", (int)response_len, response);
    }
    return JS_NewStringLen(context, response, response_len);
}

int lite_qjs_install_bridge(JSContext *context, void *opaque) {
    JSValue global = JS_GetGlobalObject(context);
    JSValue function = JS_NewCFunction(context, lite_qjs_native, "__liteNative", 2);
    JS_SetContextOpaque(context, opaque);
    int status = JS_SetPropertyStr(context, global, "__liteNative", function);
    JS_FreeValue(context, global);
    return status;
}

void lite_qjs_free_value(JSContext *context, JSValue value) {
    JS_FreeValue(context, value);
}
