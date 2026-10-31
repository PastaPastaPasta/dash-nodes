import { test } from "node:test";
import assert from "node:assert/strict";
import { authorization } from "../scripts/r2.mjs";

// The "GET Object" example from AWS's Signature Version 4 documentation for S3
// (Authenticating Requests: Using the Authorization Header).
test("signs the AWS S3 GET example", () => {
  const got = authorization({
    method: "GET",
    path: "/test.txt",
    headers: {
      host: "examplebucket.s3.amazonaws.com",
      range: "bytes=0-9",
      "x-amz-content-sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
      "x-amz-date": "20130524T000000Z",
    },
    accessKeyId: "AKIAIOSFODNN7EXAMPLE",
    secretAccessKey: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    region: "us-east-1",
  });
  assert.equal(got,
    "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, " +
    "SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, " +
    "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
});
