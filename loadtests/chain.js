import http from "k6/http";
import { check } from "k6";

const baseUrl = __ENV.BASE_URL || "http://127.0.0.1:8081";
const token = __ENV.TRADER_TOKEN || "";
export const options = { vus: Number(__ENV.K6_VUS || 20), duration: __ENV.K6_DURATION || "1m", thresholds: { http_req_duration: ["p(95)<1000"], http_req_failed: ["rate<0.05"] } };

export default function () {
  const response = http.get(`${baseUrl}/api/v1/chain/transactions/${__VU}`, { headers: { Authorization: `Bearer ${token}` } });
  check(response, { "chain API remains bounded": (result) => [200, 404, 429, 503].includes(result.status) });
}
