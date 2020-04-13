<h1 align="center">zuse</h1>

<h5 align="center">The flexible uptime bot, a descendant of the Rust async masterrace.</h5>

<div align="center">
  <a href="https://crates.io/crates/zuse">
    crates.io
  </a>
  —
  <a href="https://github.com/19h/zuse">
    Github
  </a>
</div>

<br />

```shell script
$ cargo install zuse
$ zuse -c tests.yml
```

#### Example configuration

```yaml
notifiers:
  - type: telegram
    auth:
      token: xxxx
    channels:
      - name: tg_chan
        # channel or group or user who
        # previous messaged the bot
        id: -1000000000000
  - type: aws_sns
    auth:
      key: AKIXXXXXXXXXXXXXXXXX
      secret: XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX
      region: us-east-1
    channels:
      - name: sns_pavel
        phone: +491701234567
        # or
        target_arn: arn:aws:sns:us-east-1:XXXXXXXXXXXX:XXXXXXXX
        # or
        topic_arn: arn:aws:sns:us-east-1:XXXXXXXXXXXX:XXXXXXXX

tests:
  - type: alive
    name: site-com-alive-cdn
    retries: 3
    recovery: 3
    interval: 1
    url: https://site.com/endpoint
    notify:
      - sns_pavel
      - tg_chan
```

#### Notes

`cargo` requires a rust installation.

#### License

~~ MIT License ~~

Copyright (c) 2020 Kenan Sulayman

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
