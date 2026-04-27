#include "sedsprintf.h"
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

static bool view_eq(const char * ptr, size_t len, const char * expected)
{
    return ptr != NULL && len == strlen(expected) && memcmp(ptr, expected, len) == 0;
}

static SedsResult noop_tx(const uint8_t *bytes, size_t len, void *user)
{
    (void)bytes;
    (void)len;
    (void)user;
    return SEDS_OK;
}

int main(void)
{
    const char ep_name[] = "C_RUNTIME_SCHEMA_EP_9100";
    const char ep_desc[] = "C runtime schema endpoint";
    const char ty_name[] = "C_RUNTIME_SCHEMA_TYPE_9101";
    const char ty_desc[] = "C runtime schema type";
    const uint32_t ep_id = 9100;
    const uint32_t ty_id = 9101;

    (void) seds_dtype_remove_by_name(ty_name, strlen(ty_name));
    (void) seds_endpoint_remove_by_name(ep_name, strlen(ep_name));

    SedsEndpointInfo missing_ep;
    assert(seds_endpoint_get_info_by_name("C_RUNTIME_SCHEMA_MISSING_EP",
                                          strlen("C_RUNTIME_SCHEMA_MISSING_EP"),
                                          &missing_ep)
           == SEDS_OK);
    assert(!missing_ep.exists);

    assert(seds_endpoint_register_ex(ep_id,
                                     ep_name,
                                     strlen(ep_name),
                                     ep_desc,
                                     strlen(ep_desc),
                                     true)
           == SEDS_OK);
    assert(seds_endpoint_exists(ep_id));

    SedsEndpointInfo ep_info;
    assert(seds_endpoint_get_info_by_name(ep_name, strlen(ep_name), &ep_info) == SEDS_OK);
    assert(ep_info.exists);
    assert(ep_info.id == ep_id);
    assert(ep_info.link_local_only);
    assert(view_eq(ep_info.name, ep_info.name_len, ep_name));
    assert(view_eq(ep_info.description, ep_info.description_len, ep_desc));

    const uint32_t endpoints[] = {ep_id};
    assert(seds_dtype_register_ex(ty_id,
                                  ty_name,
                                  strlen(ty_name),
                                  ty_desc,
                                  strlen(ty_desc),
                                  true,
                                  2,
                                  3, /* UInt16 */
                                  2, /* Warning */
                                  1, /* Ordered */
                                  66,
                                  endpoints,
                                  1)
           == SEDS_OK);
    assert(seds_dtype_exists(ty_id));
    assert(seds_dtype_expected_size(ty_id) == 4);

    uint32_t endpoints_out[4] = {0};
    SedsDataTypeInfo ty_info;
    assert(seds_dtype_get_info_by_name(ty_name,
                                       strlen(ty_name),
                                       endpoints_out,
                                       4,
                                       &ty_info)
           == SEDS_OK);
    assert(ty_info.exists);
    assert(ty_info.id == ty_id);
    assert(ty_info.is_static);
    assert(ty_info.element_count == 2);
    assert(ty_info.message_data_type == 3);
    assert(ty_info.message_class == 2);
    assert(ty_info.reliable == 1);
    assert(ty_info.priority == 66);
    assert(ty_info.fixed_size == 4);
    assert(ty_info.num_endpoints == 1);
    assert(endpoints_out[0] == ep_id);
    assert(view_eq(ty_info.name, ty_info.name_len, ty_name));
    assert(view_eq(ty_info.description, ty_info.description_len, ty_desc));

    assert(seds_dtype_register(ty_id,
                               ty_name,
                               strlen(ty_name),
                               true,
                               3,
                               3,
                               2,
                               1,
                               66,
                               endpoints,
                               1)
           == SEDS_BAD_ARG);

    const char json[] =
        "{"
        "\"endpoints\":[{"
        "\"rust\":\"CRuntimeJsonEp\","
        "\"name\":\"C_RUNTIME_JSON_EP\","
        "\"description\":\"C JSON endpoint\""
        "}],"
        "\"types\":[{"
        "\"rust\":\"CRuntimeJsonType\","
        "\"name\":\"C_RUNTIME_JSON_TYPE\","
        "\"description\":\"C JSON type\","
        "\"priority\":23,"
        "\"reliable_mode\":\"Unordered\","
        "\"class\":\"Data\","
        "\"element\":{\"kind\":\"Static\",\"data_type\":\"Float32\",\"count\":1},"
        "\"endpoints\":[\"CRuntimeJsonEp\"]"
        "}]"
        "}";
    (void) seds_dtype_remove_by_name("C_RUNTIME_JSON_TYPE", strlen("C_RUNTIME_JSON_TYPE"));
    (void) seds_endpoint_remove_by_name("C_RUNTIME_JSON_EP", strlen("C_RUNTIME_JSON_EP"));
    assert(seds_schema_register_json_bytes((const uint8_t *) json, strlen(json)) == SEDS_OK);

    SedsEndpointInfo json_ep;
    assert(seds_endpoint_get_info_by_name("C_RUNTIME_JSON_EP",
                                          strlen("C_RUNTIME_JSON_EP"),
                                          &json_ep)
           == SEDS_OK);
    assert(json_ep.exists);
    assert(view_eq(json_ep.description, json_ep.description_len, "C JSON endpoint"));

    uint32_t json_eps[2] = {0};
    SedsDataTypeInfo json_ty;
    assert(seds_dtype_get_info_by_name("C_RUNTIME_JSON_TYPE",
                                       strlen("C_RUNTIME_JSON_TYPE"),
                                       json_eps,
                                       2,
                                       &json_ty)
           == SEDS_OK);
    assert(json_ty.exists);
    assert(json_ty.message_data_type == 1);
    assert(json_ty.reliable == 2);
    assert(json_ty.priority == 23);
    assert(json_ty.num_endpoints == 1);
    assert(json_eps[0] == json_ep.id);

    assert(seds_dtype_remove_by_name("C_RUNTIME_JSON_TYPE", strlen("C_RUNTIME_JSON_TYPE")) == SEDS_OK);
    assert(seds_endpoint_remove_by_name("C_RUNTIME_JSON_EP", strlen("C_RUNTIME_JSON_EP")) == SEDS_OK);
    assert(seds_dtype_remove(ty_id) == SEDS_OK);
    assert(seds_endpoint_remove(ep_id) == SEDS_OK);
    assert(!seds_dtype_exists(ty_id));
    assert(!seds_endpoint_exists(ep_id));

    SedsRelay *relay = seds_relay_new(NULL, NULL);
    assert(relay != NULL);
    assert(seds_relay_add_side_serialized(relay, "SIDE_A", strlen("SIDE_A"), noop_tx, NULL, false) >= 0);
    int32_t runtime_len = seds_relay_export_runtime_stats_len(relay);
    assert(runtime_len > 0);
    char *runtime_json = (char *)malloc((size_t)runtime_len);
    assert(runtime_json != NULL);
    assert(seds_relay_export_runtime_stats(relay, runtime_json, (size_t)runtime_len) == SEDS_OK);
    assert(strstr(runtime_json, "\"sides\":[") != NULL);
    assert(strstr(runtime_json, "\"route_modes\":[") != NULL);
    assert(strstr(runtime_json, "\"queues\":{") != NULL);
    assert(strstr(runtime_json, "\"reliable\":{") != NULL);
    free(runtime_json);
    seds_relay_free(relay);

    printf("runtime schema C ABI ok\n");
    return 0;
}
