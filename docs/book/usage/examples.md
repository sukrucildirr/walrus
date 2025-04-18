# Examples

As inspiration, we provide several simple examples in different programming languages to interact
with Walrus through the various interfaces. They are located at
<https://github.com/MystenLabs/walrus/tree/main/docs/examples> and described below.

In addition, we have built actual applications on top of Walrus. The prime example is [Walrus
Sites](../walrus-sites/intro.md), with code available in the
<https://github.com/MystenLabs/walrus-sites> repository.

And for an example of how to build a static website and store it as a Walrus Site with GitHub
actions, just look at the [CI
workflow](https://github.com/MystenLabs/walrus-docs/blob/main/.github/workflows/publish.yaml) we use
to publish this very site. There is also a [Walrus-Sites GitHub
Action](https://github.com/zktx-io/walrus-sites-ga) created by the community you can use to very
easily publish your own Walrus Sites using GitHub Actions (note that this is *not* created or
officially supported by the Walrus team at Mysten Labs).

See also our list of existing and upcoming [SDKs and other tools](./sdks.md).

## Python

The [Python examples](https://github.com/MystenLabs/walrus/tree/main/docs/examples/python) folder
contains a number of examples:

- How to [use the HTTP
  API](https://github.com/MystenLabs/walrus/blob/main/docs/examples/python/hello_walrus_webapi.py)
  to store and read a blob.
- How to [use the JSON
  API](https://github.com/MystenLabs/walrus/blob/main/docs/examples/python/hello_walrus_jsonapi.py)
  to store, read, and check the availability of a blob. Checking the certification of a blob
  illustrates reading the Blob Sui object that certifies (see the [Walrus Sui
  reference](../dev-guide/sui-struct.md)).
- How to [track Walrus related
  Events](https://github.com/MystenLabs/walrus/blob/main/examples/docs/python/track_walrus_events.py).

## JavaScript

A [JavaScript example](https://github.com/MystenLabs/walrus/tree/main/docs/examples/javascript) is
provided showing how to upload and download a blob through a web form using the HTTP API.

## Move

For more complex applications, you may want to interact with Walrus on-chain objects. For that
purpose, the currently deployed Walrus contracts are included [in our GitHub
repository](https://github.com/MystenLabs/walrus/tree/main/contracts).

Furthermore, we provide a simple [example
contract](https://github.com/MystenLabs/walrus/tree/main/docs/examples/move) that imports and uses
the Walrus objects.
