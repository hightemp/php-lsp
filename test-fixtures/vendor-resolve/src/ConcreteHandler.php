<?php

namespace App;

/**
 * Concrete handler extending BaseHandler.
 * Tests go-to-definition on inherited method okResponse from parent.
 */
class ConcreteHandler extends BaseHandler
{
    public function handle(): array
    {
        // Case A: go-to-definition on okResponse → BaseHandler::okResponse (inherited method)
        return $this->okResponse(['key' => 'value']);
    }

    public function handleWithTimer(): void
    {
        // Case B: go-to-definition on start → TimerService::start
        // $this->timer is declared in BaseHandler (parent class, different file)
        // This tests Bug 3: cross-file property type resolution
        $this->timer->start('handle');
    }
}
